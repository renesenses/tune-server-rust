//! L'écoute à distance par compte, de bout en bout (#7714, #7833, #7558).
//!
//! Tout est vrai sauf le site et l'API locale du serveur :
//!
//! - le VRAI relais (`crate::routes()`) sur 127.0.0.1, avec ses sessions par
//!   laissez-passer ;
//! - le VRAI client de relais du serveur (`tune_core::cloud::relay`, lancé
//!   par `spawn_relay_client` depuis des réglages, comme au démarrage de
//!   `tune-server`), avec sa VRAIE clé (coffre) et la VRAIE inscription de
//!   la clé sur le compte (`tune_core::cloud::cle_serveur`) ;
//! - un FAUX SITE qui rejoue les règles de `LinkCloudServer`,
//!   `UnlinkCloudServer`, `RemoteServerController`, `RemoteDeviceController`
//!   et `IssueRemotePass` (site-mozaiklabs), et signe les laissez-passer avec
//!   les clés de TEST publiées ;
//! - une fausse API locale (`/api/v1/...`, `/stream/{id}`) derrière le
//!   serveur : ce qui est éprouvé ici, c'est le chemin, pas la bibliothèque.
//!
//! L'app est jouée par un client HTTP qui suit le parcours d'« Écoute à
//! distance » : appareil enregistré sur le compte, liste des serveurs du
//! compte, laissez-passer, défi, session, puis navigation et lecture à
//! travers le relais. Aucun appel réseau sortant.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tune_core::cloud::cle_serveur::{self, CorpsInscription, InscriptionDeCle};
use tune_core::cloud::library_sync::CLE_JETON_DE_LIAISON;
use tune_core::cloud::relay::RelayClient;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_laissez_passer::Publication;
use tune_laissez_passer::fabrique::{AppareilDeTest, cles_de_test, revendications};

use crate::sessions::{Sessions, horloge_systeme};
use crate::site::{
    Battement, DemandeOuverture, ReponseBattement, ReponseOuverture, Site, SiteInjoignable,
};
use crate::state::RelayState;

const PATIENCE: Duration = Duration::from_secs(10);
/// Comme le site : cinq appareils actifs par compte.
const APPAREILS_MAX: usize = 5;
const COMPTE: &str = "compte-du-testeur";

// ---------------------------------------------------------------------------
// Le faux site
// ---------------------------------------------------------------------------

struct Liaison {
    server_id: String,
    compte: String,
    jeton_de_liaison: String,
}

#[derive(Default)]
struct EtatDuSite {
    /// `cloud_servers`, dans l'ordre de `claimed_at`.
    serveurs: Vec<Liaison>,
    /// `cloud_server_keys` : NON effacée par la déliaison, comme sur le site.
    cles: HashMap<String, String>,
    /// `remote_devices` : (compte, device_id, clé publique).
    appareils: Vec<(String, String, String)>,
    sessions_ouvertes: u64,
    jti: u64,
}

#[derive(Default)]
struct FauxSite {
    etat: Mutex<EtatDuSite>,
}

/// Le jeton OAuth du compte, tel que le serveur le présente.
fn jeton_oauth(compte: &str) -> String {
    format!("oauth:{compte}")
}

impl FauxSite {
    /// `POST /api/v1/cloud-library/{server_id}/link` (LinkCloudServer).
    fn lier(&self, compte: &str, server_id: &str) -> Option<String> {
        let mut e = self.etat.lock().unwrap();
        let jeton = format!(
            "liaison-{}-{}",
            server_id,
            e.serveurs.len() + e.jti as usize
        );
        e.jti += 1;
        match e.serveurs.iter_mut().find(|l| l.server_id == server_id) {
            Some(l) if l.compte != compte => None,
            Some(l) => {
                l.jeton_de_liaison = jeton.clone();
                Some(jeton)
            }
            None => {
                e.serveurs.push(Liaison {
                    server_id: server_id.to_string(),
                    compte: compte.to_string(),
                    jeton_de_liaison: jeton.clone(),
                });
                Some(jeton)
            }
        }
    }

    /// `DELETE /api/v1/cloud-library/{server_id}/link` (UnlinkCloudServer) :
    /// la liaison disparaît, la clé du serveur reste.
    fn delier(&self, compte: &str, server_id: &str) {
        let mut e = self.etat.lock().unwrap();
        e.serveurs
            .retain(|l| !(l.server_id == server_id && l.compte == compte));
    }

    /// `POST /api/v1/remote/devices` : refusé au-delà de cinq appareils.
    fn enregistrer_appareil(
        &self,
        compte: &str,
        did: &str,
        appareil: &AppareilDeTest,
    ) -> Result<(), &'static str> {
        let mut e = self.etat.lock().unwrap();
        if e.appareils.iter().any(|(_, d, _)| d == did) {
            return Ok(());
        }
        if e.appareils.iter().filter(|(c, _, _)| c == compte).count() >= APPAREILS_MAX {
            return Err("limite_appareils");
        }
        e.appareils
            .push((compte.into(), did.into(), appareil.confirmation().pk));
        Ok(())
    }

    /// `GET /api/v1/remote/servers` : (server_id, srv_pk), ordre de liaison.
    fn serveurs_du_compte(&self, compte: &str) -> Vec<(String, Option<String>)> {
        let e = self.etat.lock().unwrap();
        e.serveurs
            .iter()
            .filter(|l| l.compte == compte)
            .map(|l| (l.server_id.clone(), e.cles.get(&l.server_id).cloned()))
            .collect()
    }

    /// `POST /api/v1/remote/passes` (IssueRemotePass), sans la signature de
    /// la demande par l'appareil (éprouvée côté site).
    fn laissez_passer(
        &self,
        compte: &str,
        did: &str,
        appareil: &AppareilDeTest,
        server_id: &str,
    ) -> Result<String, &'static str> {
        let mut e = self.etat.lock().unwrap();
        if !e.appareils.iter().any(|(c, d, _)| c == compte && d == did) {
            return Err("appareil_inconnu");
        }
        match e.serveurs.iter().find(|l| l.server_id == server_id) {
            None => return Err("serveur_non_relie"),
            Some(l) if l.compte != compte => return Err("serveur_autre_compte"),
            Some(_) => {}
        }
        e.jti += 1;
        let mut r = revendications(
            compte,
            server_id,
            did,
            appareil.confirmation(),
            chrono_maintenant(),
            &format!("lp_{}", e.jti),
        );
        r.srv_pk = e.cles.get(server_id).cloned();
        Ok(cles_de_test::site().signer(&r))
    }
}

fn chrono_maintenant() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// `POST /api/v1/remote/servers/{server_id}/key` (RemoteServerController).
#[async_trait]
impl InscriptionDeCle for FauxSite {
    async fn inscrire(
        &self,
        server_id: &str,
        jeton_du_compte: &str,
        jeton_de_liaison: &str,
        corps: &CorpsInscription,
    ) -> Result<(), String> {
        let mut e = self.etat.lock().unwrap();
        let Some(l) = e.serveurs.iter().find(|l| l.server_id == server_id) else {
            return Err("inscription de la cle : HTTP 403 serveur_non_relie".into());
        };
        if jeton_oauth(&l.compte) != jeton_du_compte {
            return Err("inscription de la cle : HTTP 403 serveur_non_relie".into());
        }
        if l.jeton_de_liaison != jeton_de_liaison {
            return Err("inscription de la cle : HTTP 403 jeton_serveur".into());
        }
        tune_laissez_passer::verifier_inscription_serveur(
            &corps.public_key,
            server_id,
            &corps.preuve,
        )
        .map_err(|_| "inscription de la cle : HTTP 403 preuve".to_string())?;
        e.cles
            .insert(server_id.to_string(), corps.public_key.clone());
        Ok(())
    }
}

/// Ce que le relais demande au site.
#[async_trait]
impl Site for FauxSite {
    async fn ouvrir_session(
        &self,
        _d: &DemandeOuverture,
    ) -> Result<ReponseOuverture, SiteInjoignable> {
        let mut e = self.etat.lock().unwrap();
        e.sessions_ouvertes += 1;
        Ok(ReponseOuverture {
            ok: true,
            session_id: Some(format!("s{}", e.sessions_ouvertes)),
            ..Default::default()
        })
    }

    async fn battement(&self, _corps: &Battement) -> Result<ReponseBattement, SiteInjoignable> {
        Ok(ReponseBattement::default())
    }

    async fn jeu_de_cles(&self) -> Result<Publication, SiteInjoignable> {
        Ok(cles_de_test::publication())
    }

    async fn cle_du_serveur(&self, server_id: &str) -> Result<Option<String>, SiteInjoignable> {
        Ok(self.etat.lock().unwrap().cles.get(server_id).cloned())
    }
}

// ---------------------------------------------------------------------------
// Le relais, l'API locale, le serveur
// ---------------------------------------------------------------------------

struct Relais {
    adresse: std::net::SocketAddr,
    state: Arc<RelayState>,
}

async fn relais(site: Arc<FauxSite>) -> Relais {
    let sessions = Sessions::nouveau(
        site,
        &cles_de_test::racine().cle_publique_minisign(),
        false,
        horloge_systeme(),
    );
    sessions.rafraichir_jeu().await.unwrap();
    let mut state = RelayState::new();
    state.sessions = sessions;
    let state = Arc::new(state);
    let app = crate::routes().with_state(state.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    Relais { adresse, state }
}

/// Octets du « morceau » servi par l'API locale.
fn morceau() -> Vec<u8> {
    (0..200_000u32).map(|i| (i % 251) as u8).collect()
}

/// La fausse API locale du serveur : une liste d'albums et un flux.
async fn api_locale(nom: &'static str) -> u16 {
    use axum::routing::get;
    let app = axum::Router::new()
        .route(
            "/api/v1/library/albums",
            get(move || async move {
                axum::Json(serde_json::json!({"albums": [{"id": 1, "title": nom}]}))
            }),
        )
        .route(
            "/stream/{id}",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "audio/flac")],
                    morceau(),
                )
            }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    port
}

/// Une installation de Tune : sa base, son identifiant, son client de relais.
struct Serveur {
    backend: Arc<dyn DbBackend>,
    server_id: String,
    client: Arc<RelayClient>,
}

fn base() -> Arc<dyn DbBackend> {
    let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Démarre un serveur NEUF (nouvelle base : nouvel identifiant, nouvelle
/// clé), pont activé vers le relais de test, comme `tune-server` au
/// démarrage (`background::spawn_relay_client`).
async fn demarrer_serveur(relais: &Relais, nom: &'static str) -> Serveur {
    let backend = base();
    let reglages = SettingsRepo::with_backend(backend.clone());
    reglages.set("bridge_enabled", "true").unwrap();
    reglages
        .set("bridge_url", &format!("ws://{}/ws/server", relais.adresse))
        .unwrap();
    reglages.set("server_name", nom).unwrap();
    let port = api_locale(nom).await;
    let client = tune_core::cloud::relay::spawn_relay_client(backend.clone(), port)
        .expect("pont active : le client de relais doit demarrer");
    let server_id = reglages.get("server_id").unwrap().unwrap();
    let s = Serveur {
        backend,
        server_id,
        client,
    };
    s.attendre_connexion(relais).await;
    s
}

impl Serveur {
    fn reglages(&self) -> SettingsRepo {
        SettingsRepo::with_backend(self.backend.clone())
    }

    async fn attendre_connexion(&self, relais: &Relais) {
        tokio::time::timeout(PATIENCE, async {
            while !(self.client.is_connected()
                && relais.state.servers.contains_key(&self.server_id))
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "le serveur {} ne s'est pas inscrit au relais",
                self.server_id
            )
        });
    }

    /// Connexion au compte dans les Réglages (`/cloud/sso/callback`) :
    /// jetons rangés, liaison au compte, puis inscription de la clé.
    async fn connecter_au_compte(&self, site: &FauxSite, compte: &str) {
        let r = self.reglages();
        r.set("mozaik_access_token", &jeton_oauth(compte)).unwrap();
        let liaison = site
            .lier(compte, &self.server_id)
            .expect("le site refuse la liaison");
        r.set(CLE_JETON_DE_LIAISON, &liaison).unwrap();
        cle_serveur::inscrire_si_besoin(&self.backend, site)
            .await
            .expect("inscription de la cle du serveur");
    }

    /// Déconnexion dans les Réglages (`/cloud/sso/disconnect`) : déliaison
    /// chez le site, puis effacement des jetons.
    fn deconnecter_du_compte(&self, site: &FauxSite, compte: &str) {
        site.delier(compte, &self.server_id);
        let r = self.reglages();
        for cle in [
            "mozaik_access_token",
            "mozaik_refresh_token",
            "mozaik_user",
            CLE_JETON_DE_LIAISON,
        ] {
            r.delete(cle).unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// L'app
// ---------------------------------------------------------------------------

struct App {
    http: reqwest::Client,
    relais: std::net::SocketAddr,
    appareil: AppareilDeTest,
    did: String,
}

#[derive(Debug)]
struct Session {
    server_id: String,
    jeton: String,
}

impl App {
    fn nouvelle(relais: &Relais, graine: &[u8; 32], did: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            relais: relais.adresse,
            appareil: AppareilDeTest::ed25519(graine),
            did: did.to_string(),
        }
    }

    /// Ouvre une session sur `server_id` : laissez-passer, défi, signature.
    async fn ouvrir(&self, site: &FauxSite, server_id: &str) -> Result<Session, String> {
        let lp = site
            .laissez_passer(COMPTE, &self.did, &self.appareil, server_id)
            .map_err(|e| format!("laissez-passer refuse : {e}"))?;
        let r = self
            .http
            .get(format!("http://{}/session/defi/{server_id}", self.relais))
            .send()
            .await
            .unwrap();
        let statut = r.status().as_u16();
        let corps: serde_json::Value = r.json().await.unwrap_or_default();
        if statut != 200 {
            return Err(format!("defi {statut} {corps}"));
        }
        let defi = corps["defi"].as_str().unwrap().to_string();
        let r = self
            .http
            .post(format!("http://{}/session/{server_id}", self.relais))
            .json(&serde_json::json!({
                "laissez_passer": lp,
                "defi": defi,
                "signature": self.appareil.signer_defi(server_id, &defi),
            }))
            .send()
            .await
            .unwrap();
        let statut = r.status().as_u16();
        let corps: serde_json::Value = r.json().await.unwrap_or_default();
        if statut != 200 {
            return Err(format!("session {statut} {corps}"));
        }
        Ok(Session {
            server_id: server_id.to_string(),
            jeton: corps["jeton"].as_str().unwrap().to_string(),
        })
    }

    /// Le serveur à ouvrir, parmi ceux du compte : le premier qui répond au
    /// défi du relais, les plus récemment liés d'abord (un serveur
    /// réinstallé a un nouvel identifiant, l'ancien reste sur le compte).
    async fn choisir_et_ouvrir(&self, site: &FauxSite) -> Result<Session, String> {
        let mut erreurs = Vec::new();
        for (sid, _) in site.serveurs_du_compte(COMPTE).into_iter().rev() {
            match self.ouvrir(site, &sid).await {
                Ok(s) => return Ok(s),
                Err(e) => erreurs.push(format!("{sid} : {e}")),
            }
        }
        Err(erreurs.join(" ; "))
    }

    /// Navigation : la liste des albums, à travers le relais (qui préfixe
    /// `/api/v1`, comme le font les apps).
    async fn albums(&self, s: &Session) -> (u16, serde_json::Value) {
        let r = self
            .http
            .get(format!(
                "http://{}/api/relay/{}/library/albums",
                self.relais, s.server_id
            ))
            .header("x-bridge-token", &s.jeton)
            .send()
            .await
            .unwrap();
        let statut = r.status().as_u16();
        (statut, r.json().await.unwrap_or_default())
    }

    /// Lecture : le flux entier, à travers le relais.
    async fn lire(&self, s: &Session) -> (u16, Vec<u8>) {
        let r = self
            .http
            .get(format!(
                "http://{}/stream/relay/{}/42",
                self.relais, s.server_id
            ))
            .header("x-bridge-token", &s.jeton)
            .send()
            .await
            .unwrap();
        let statut = r.status().as_u16();
        (statut, r.bytes().await.unwrap_or_default().to_vec())
    }

    async fn naviguer_et_lire(&self, s: &Session, nom: &str, quand: &str) {
        let (statut, albums) = tokio::time::timeout(PATIENCE, self.albums(s))
            .await
            .unwrap_or_else(|_| panic!("{quand} : la navigation ne repond pas"));
        assert_eq!(statut, 200, "{quand} : navigation refusee : {albums}");
        assert_eq!(
            albums["albums"][0]["title"], nom,
            "{quand} : mauvais serveur"
        );
        let (statut, octets) = tokio::time::timeout(PATIENCE, self.lire(s))
            .await
            .unwrap_or_else(|_| panic!("{quand} : le flux ne vient pas"));
        assert_eq!(statut, 200, "{quand} : lecture refusee");
        assert!(
            octets == morceau(),
            "{quand} : flux tronque ({} octets)",
            octets.len()
        );
    }
}

async fn banc() -> (Arc<FauxSite>, Relais) {
    let site = Arc::new(FauxSite::default());
    let r = relais(site.clone()).await;
    (site, r)
}

// ---------------------------------------------------------------------------
// Le parcours
// ---------------------------------------------------------------------------

/// Le parcours nominal : serveur relié, appareil enregistré, session,
/// navigation, lecture.
#[tokio::test]
async fn parcours_complet_liaison_appareil_session_navigation_lecture() {
    let (site, relais) = banc().await;
    let serveur = demarrer_serveur(&relais, "Salon").await;

    // Le relais a annoncé le laissez-passer : sans cela, la clé ne s'inscrit
    // jamais sur le compte.
    assert!(
        cle_serveur::laissez_passer_actif(&serveur.reglages()),
        "le relais n'a pas annonce le laissez-passer au serveur"
    );
    serveur.connecter_au_compte(&site, COMPTE).await;
    let serveurs = site.serveurs_du_compte(COMPTE);
    assert_eq!(serveurs.len(), 1);
    assert!(
        serveurs[0].1.is_some(),
        "la cle du serveur n'est pas inscrite sur le compte"
    );

    let app = App::nouvelle(&relais, &cles_de_test::GRAINE_APPAREIL, "did-telephone");
    site.enregistrer_appareil(COMPTE, &app.did, &app.appareil)
        .unwrap();
    let session = app.choisir_et_ouvrir(&site).await.expect("session refusee");
    assert_eq!(session.server_id, serveur.server_id);
    app.naviguer_et_lire(&session, "Salon", "parcours nominal")
        .await;
}

/// Sans appareil enregistré sur le compte (« 0 appareil sur 5 »), aucun
/// laissez-passer : c'est le premier point de rupture à regarder.
#[tokio::test]
async fn sans_appareil_enregistre_aucun_laissez_passer() {
    let (site, relais) = banc().await;
    let serveur = demarrer_serveur(&relais, "Salon").await;
    serveur.connecter_au_compte(&site, COMPTE).await;
    let app = App::nouvelle(&relais, &cles_de_test::GRAINE_APPAREIL, "did-telephone");
    let refus = app.choisir_et_ouvrir(&site).await.unwrap_err();
    assert!(refus.contains("appareil_inconnu"), "{refus}");
}

/// Serveur réinstallé : nouvel identifiant, nouvelle clé, l'ancien reste
/// sur le compte sans être connecté. L'app doit atteindre le nouveau.
#[tokio::test]
async fn serveur_reinstalle_l_ancien_reste_sur_le_compte() {
    let (site, relais) = banc().await;
    let ancien = demarrer_serveur(&relais, "Ancien").await;
    ancien.connecter_au_compte(&site, COMPTE).await;
    let ancien_id = ancien.server_id.clone();
    // L'ancienne installation disparaît (disque effacé) : plus de connexion.
    drop(ancien);
    relais.state.unregister_server(&ancien_id);

    let nouveau = demarrer_serveur(&relais, "Nouveau").await;
    assert_ne!(nouveau.server_id, ancien_id);
    nouveau.connecter_au_compte(&site, COMPTE).await;
    assert_eq!(site.serveurs_du_compte(COMPTE).len(), 2);

    let app = App::nouvelle(&relais, &cles_de_test::GRAINE_APPAREIL, "did-telephone");
    site.enregistrer_appareil(COMPTE, &app.did, &app.appareil)
        .unwrap();
    // L'ancien, demandé directement, répond « serveur absent » : c'est ce que
    // lit une app qui choisirait le premier serveur de la liste.
    let refus = app.ouvrir(&site, &ancien_id).await.unwrap_err();
    assert!(refus.contains("serveur_absent"), "{refus}");
    let session = app.choisir_et_ouvrir(&site).await.expect("session refusee");
    assert_eq!(session.server_id, nouveau.server_id);
    app.naviguer_et_lire(&session, "Nouveau", "serveur reinstalle")
        .await;
}

/// Déconnexion puis reconnexion du compte dans les Réglages : la liaison est
/// refaite (nouveau jeton), la clé reste connue du site, l'écoute reprend.
#[tokio::test]
async fn deconnexion_puis_reconnexion_du_compte() {
    let (site, relais) = banc().await;
    let serveur = demarrer_serveur(&relais, "Salon").await;
    serveur.connecter_au_compte(&site, COMPTE).await;
    let app = App::nouvelle(&relais, &cles_de_test::GRAINE_APPAREIL, "did-telephone");
    site.enregistrer_appareil(COMPTE, &app.did, &app.appareil)
        .unwrap();

    serveur.deconnecter_du_compte(&site, COMPTE);
    assert!(site.serveurs_du_compte(COMPTE).is_empty());
    let refus = app.choisir_et_ouvrir(&site).await.unwrap_err();
    assert!(
        refus.is_empty(),
        "aucun serveur ne doit etre propose : {refus}"
    );

    serveur.connecter_au_compte(&site, COMPTE).await;
    let serveurs = site.serveurs_du_compte(COMPTE);
    assert_eq!(serveurs.len(), 1);
    assert!(
        serveurs[0].1.is_some(),
        "cle du serveur perdue a la reliaison"
    );
    let session = app.choisir_et_ouvrir(&site).await.expect("session refusee");
    app.naviguer_et_lire(&session, "Salon", "apres reconnexion du compte")
        .await;
}

/// Le relais coupe la connexion du serveur (redéploiement, box qui
/// redémarre) plusieurs fois de suite : le serveur doit revenir en
/// quelques secondes CHAQUE fois, et la session de l'app doit continuer.
///
/// Avant le correctif, chaque coupure après une inscription réussie
/// allongeait l'attente (2, 4, 8, 16, 32 puis 60 s) : au troisième
/// redéploiement du relais, l'app lisait « serveur absent » pendant 8 s,
/// et une minute entière sur un serveur qui tournait depuis des jours.
#[tokio::test]
async fn coupures_breves_le_serveur_revient_vite_a_chaque_fois() {
    let (site, relais) = banc().await;
    let serveur = demarrer_serveur(&relais, "Salon").await;
    serveur.connecter_au_compte(&site, COMPTE).await;
    let app = App::nouvelle(&relais, &cles_de_test::GRAINE_APPAREIL, "did-telephone");
    site.enregistrer_appareil(COMPTE, &app.did, &app.appareil)
        .unwrap();
    let session = app.choisir_et_ouvrir(&site).await.expect("session refusee");

    for n in 1..=4 {
        relais.state.unregister_server(&serveur.server_id);
        // Le client voit la fermeture, puis se réinscrit.
        tokio::time::timeout(Duration::from_secs(2), async {
            while serveur.client.is_connected() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("coupure {n} : le serveur n'a pas vu la fermeture"));
        let debut = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(4), async {
            while !relais.state.servers.contains_key(&serveur.server_id) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "coupure {n} : le serveur n'est pas revenu au relais en 4 s \
                 (attente croissante apres une inscription reussie)"
            )
        });
        serveur.attendre_connexion(&relais).await;
        assert!(debut.elapsed() < Duration::from_secs(4));
        app.naviguer_et_lire(&session, "Salon", &format!("apres la coupure {n}"))
            .await;
    }
}

// ---------------------------------------------------------------------------
// « État : Déconnecté » sans retour (#7714, constat du 09/11 15 h 08)
// ---------------------------------------------------------------------------

/// Un serveur relié dont la clé du coffre a changé (coffre effacé, base
/// restaurée) : le site connaît l'ANCIENNE clé, le relais refuse toute autre
/// (« cle du serveur requise »). Avant, l'impasse était permanente : le
/// serveur n'inscrit sa clé qu'après une inscription réussie au relais. Il
/// doit réinscrire sa clé sur le compte et revenir seul.
#[tokio::test]
async fn cle_du_serveur_changee_le_serveur_reinscrit_sa_cle_et_revient_seul() {
    let (site, relais) = banc().await;
    let backend = base();
    let reglages = SettingsRepo::with_backend(backend.clone());
    let server_id = "a6f0c2de-7714-4c0e-9b1a-0000000000aa";
    reglages.set("server_id", server_id).unwrap();
    reglages.set("server_name", "Salon").unwrap();
    // Relié au compte (session SSO et jeton de liaison)...
    reglages
        .set("mozaik_access_token", &jeton_oauth(COMPTE))
        .unwrap();
    let liaison = site.lier(COMPTE, server_id).unwrap();
    reglages.set(CLE_JETON_DE_LIAISON, &liaison).unwrap();
    // ... mais le site garde la clé d'AVANT.
    let ancienne = tune_laissez_passer::CleServeur::depuis_graine(&[7u8; 32])
        .unwrap()
        .cle_publique();
    site.etat
        .lock()
        .unwrap()
        .cles
        .insert(server_id.to_string(), ancienne.clone());

    let cle = cle_serveur::cle_du_serveur(&backend).unwrap();
    let nouvelle = cle.cle_publique();
    assert_ne!(nouvelle, ancienne);
    let port = api_locale("Salon").await;
    let client = Arc::new(
        RelayClient::new(
            server_id.into(),
            "jeton-du-pont-7714-0123456789".into(),
            format!("ws://{}/ws/server", relais.adresse),
            port,
        )
        .avec_reglages(backend.clone())
        .avec_cle(cle)
        .avec_inscription(site.clone()),
    );
    client.clone().spawn();

    tokio::time::timeout(PATIENCE, async {
        while !(client.is_connected() && relais.state.servers.contains_key(server_id)) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "le relais refuse le serveur (cle du serveur requise) et il ne revient pas seul : \
             derniere erreur {:?}",
            client.derniere_erreur()
        )
    });
    assert_eq!(
        site.etat.lock().unwrap().cles.get(server_id),
        Some(&nouvelle),
        "la cle du serveur n'a pas ete reinscrite sur le compte"
    );
    assert_eq!(client.derniere_erreur(), None);
}

/// Un faux relais : `muet` n'accuse jamais l'inscription ; sinon il l'accuse
/// puis se tait sans fermer (connexion à moitié morte). Rend son adresse et
/// le nombre de connexions reçues.
async fn faux_relais(muet: bool) -> (std::net::SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
    use futures_util::{SinkExt, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    let connexions = Arc::new(AtomicUsize::new(0));
    let c = connexions.clone();
    tokio::spawn(async move {
        let mut gardees = Vec::new();
        while let Ok((tcp, _)) = ecoute.accept().await {
            c.fetch_add(1, Ordering::SeqCst);
            let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                continue;
            };
            if !muet {
                let _ = ws.next().await; // relay.register
                let ack = serde_json::json!({
                    "type": "relay.registered", "ok": true, "server_id": "x",
                });
                let _ = ws
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        ack.to_string().into(),
                    ))
                    .await;
            }
            // Ni réponse, ni fermeture : la socket reste ouverte.
            gardees.push(ws);
        }
    });
    (adresse, connexions)
}

fn client_vers(adresse: std::net::SocketAddr, port: u16) -> Arc<RelayClient> {
    Arc::new(
        RelayClient::new(
            "b7e0c2de-7714-4c0e-9b1a-0000000000bb".into(),
            "jeton-du-pont-7714-0123456789".into(),
            format!("ws://{adresse}/ws/server"),
            port,
        )
        .avec_delais(tune_core::cloud::relay::DelaisDuRelais {
            connexion: Duration::from_secs(1),
            poignee: Duration::from_secs(1),
            silence: Duration::from_secs(1),
        }),
    )
}

/// Un relais qui accepte la connexion sans jamais répondre (redéploiement en
/// cours, mandataire bloqué) : le serveur abandonne la tentative et
/// réessaie, au lieu d'attendre l'accusé pour toujours.
#[tokio::test]
async fn relais_muet_le_serveur_abandonne_et_reessaie() {
    let (adresse, connexions) = faux_relais(true).await;
    let client = client_vers(adresse, api_locale("Salon").await);
    client.clone().spawn();
    tokio::time::sleep(Duration::from_secs(7)).await;
    let n = connexions.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        n >= 2,
        "le serveur attend l'accuse d'un relais muet pour toujours ({n} connexion) : \
         il ne se reconnectera jamais sans redemarrage"
    );
    assert!(!client.is_connected());
    assert!(
        client
            .derniere_erreur()
            .is_some_and(|e| e.contains("timeout")),
        "la cause doit etre lisible dans les Reglages : {:?}",
        client.derniere_erreur()
    );
}

/// Une connexion inscrite que le relais cesse d'alimenter (TCP à moitié
/// ouvert après un changement d'adresse de la box, relais redéployé sans
/// fermeture vue) : le serveur la tient pour morte et se réinscrit.
#[tokio::test]
async fn relais_silencieux_le_serveur_se_reinscrit() {
    let (adresse, connexions) = faux_relais(false).await;
    let client = client_vers(adresse, api_locale("Salon").await);
    client.clone().spawn();
    tokio::time::sleep(Duration::from_secs(6)).await;
    let n = connexions.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        n >= 2,
        "une connexion que le relais n'alimente plus est gardee pour toujours ({n} \
         connexion) : le serveur se croit connecte, le relais l'a oublie"
    );
}
