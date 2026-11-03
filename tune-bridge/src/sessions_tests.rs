//! L'écoute à distance par laissez-passer vue de l'extérieur (#6603) : un
//! vrai relais sur 127.0.0.1 avec la table de routes de `main`, un vrai
//! client HTTP, un faux serveur Tune au bout du canal, et un FAUX SITE qui
//! signe avec les clés de test des vecteurs. Aucun appel réseau sortant,
//! aucun son.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tune_laissez_passer::Publication;
use tune_laissez_passer::fabrique::{AppareilDeTest, cles_de_test, revendications};

use crate::sessions::{Horloge, Sessions};
use crate::site::{
    Battement, DemandeOuverture, ReponseBattement, ReponseOuverture, Site, SiteInjoignable,
};
use crate::state::RelayState;

/// 26/10/2026 : dans la période de validité de la clé de test.
const T: i64 = 1_793_000_000;
const SID: &str = "6f1c1f5e-2b0a-4c2e-9d3b-7a51c0de0001";
const JETON_HERITE: &str = "jeton-du-pont-herite-0123456789";
const PATIENCE: Duration = Duration::from_secs(5);

#[derive(Default)]
struct FauxSite {
    injoignable: AtomicBool,
    refus: Mutex<Option<String>>,
    prochain_id: AtomicU64,
    ouvertures: Mutex<Vec<DemandeOuverture>>,
    battements: Mutex<Vec<Value>>,
    reponse_battement: Mutex<ReponseBattement>,
    noms: Mutex<Vec<(String, String)>>,
    cle_serveur: Mutex<Option<String>>,
    lectures_du_jeu: AtomicU64,
    publication: Mutex<Option<Publication>>,
}

impl FauxSite {
    fn panne(&self, oui: bool) {
        self.injoignable.store(oui, Ordering::SeqCst);
    }

    fn joindre(&self) -> Result<(), SiteInjoignable> {
        if self.injoignable.load(Ordering::SeqCst) {
            Err(SiteInjoignable("panne simulee".into()))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl Site for FauxSite {
    async fn ouvrir_session(
        &self,
        d: &DemandeOuverture,
    ) -> Result<ReponseOuverture, SiteInjoignable> {
        self.joindre()?;
        self.ouvertures.lock().unwrap().push(d.clone());
        if let Some(motif) = self.refus.lock().unwrap().clone() {
            return Ok(ReponseOuverture {
                ok: false,
                motif: Some(motif),
                ..Default::default()
            });
        }
        let n = self.prochain_id.fetch_add(1, Ordering::SeqCst) + 1;
        let appareil = self
            .noms
            .lock()
            .unwrap()
            .iter()
            .find(|(did, _)| did == &d.did)
            .map(|(_, nom)| nom.clone());
        Ok(ReponseOuverture {
            ok: true,
            session_id: Some(format!("s{n}")),
            couper: Vec::new(),
            motif: None,
            appareil,
        })
    }

    async fn battement(&self, corps: &Battement) -> Result<ReponseBattement, SiteInjoignable> {
        self.joindre()?;
        self.battements
            .lock()
            .unwrap()
            .push(serde_json::to_value(corps).unwrap());
        Ok(std::mem::take(&mut *self.reponse_battement.lock().unwrap()))
    }

    async fn jeu_de_cles(&self) -> Result<Publication, SiteInjoignable> {
        self.joindre()?;
        self.lectures_du_jeu.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .publication
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(cles_de_test::publication))
    }

    async fn cle_du_serveur(&self, _server_id: &str) -> Result<Option<String>, SiteInjoignable> {
        self.joindre()?;
        Ok(self.cle_serveur.lock().unwrap().clone())
    }
}

struct Banc {
    base: String,
    http: reqwest::Client,
    state: Arc<RelayState>,
    site: Arc<FauxSite>,
    horloge: Arc<AtomicI64>,
    serveur: mpsc::Receiver<String>,
}

async fn banc_avec(herite_ouvert: bool, racine: &str, charger_jeu: bool) -> Banc {
    let site = Arc::new(FauxSite::default());
    let horloge = Arc::new(AtomicI64::new(T));
    let h = horloge.clone();
    let source: Horloge = Arc::new(move || h.load(Ordering::SeqCst));
    let sessions = Sessions::nouveau(site.clone(), racine, herite_ouvert, source);
    if charger_jeu {
        sessions.rafraichir_jeu().await.unwrap();
    }
    let mut state = RelayState::new();
    state.sessions = sessions;
    let state = Arc::new(state);
    let (tx, serveur) = mpsc::channel::<String>(64);
    state
        .register_server(SID.into(), "Salon".into(), JETON_HERITE.into(), None, tx)
        .unwrap();
    let app = crate::routes().with_state(state.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    Banc {
        base: format!("http://{adresse}"),
        http: reqwest::Client::new(),
        state,
        site,
        horloge,
        serveur,
    }
}

async fn banc(herite_ouvert: bool) -> Banc {
    banc_avec(
        herite_ouvert,
        &cles_de_test::racine().cle_publique_minisign(),
        true,
    )
    .await
}

fn iphone() -> AppareilDeTest {
    AppareilDeTest::ed25519(&cles_de_test::GRAINE_APPAREIL)
}

fn ipad() -> AppareilDeTest {
    AppareilDeTest::ed25519(&cles_de_test::GRAINE_AUTRE_APPAREIL)
}

fn laissez_passer(appareil: &AppareilDeTest, sub: &str, did: &str, iat: i64, jti: &str) -> String {
    cles_de_test::site().signer(&revendications(
        sub,
        SID,
        did,
        appareil.confirmation(),
        iat,
        jti,
    ))
}

impl Banc {
    fn maintenant(&self) -> i64 {
        self.horloge.load(Ordering::SeqCst)
    }

    fn avancer(&self, secondes: i64) {
        self.horloge.fetch_add(secondes, Ordering::SeqCst);
    }

    async fn defi(&self) -> String {
        let r = self
            .http
            .get(format!("{}/session/defi/{SID}", self.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.json::<Value>().await.unwrap()["defi"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Ouvre une session : défi, laissez-passer, signature. Rend le statut et
    /// le corps JSON.
    async fn ouvrir_avec(&self, appareil: &AppareilDeTest, lp: &str) -> (u16, Value) {
        let defi = self.defi().await;
        let r = self
            .http
            .post(format!("{}/session/{SID}", self.base))
            .json(&json!({
                "laissez_passer": lp,
                "defi": defi,
                "signature": appareil.signer_defi(SID, &defi),
            }))
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap())
    }

    async fn ouvrir(&self, appareil: &AppareilDeTest, sub: &str, did: &str, jti: &str) -> String {
        let lp = laissez_passer(appareil, sub, did, self.maintenant(), jti);
        let (statut, corps) = self.ouvrir_avec(appareil, &lp).await;
        assert_eq!(statut, 200, "ouverture refusee : {corps}");
        corps["jeton"].as_str().unwrap().to_string()
    }

    async fn api(&self, jeton: Option<&str>) -> reqwest::Response {
        let mut r = self
            .http
            .get(format!("{}/api/relay/{SID}/library/albums", self.base));
        if let Some(j) = jeton {
            r = r.header("x-bridge-token", j);
        }
        r.send().await.unwrap()
    }

    /// Une requête d'API à laquelle le faux serveur répond. Rend la requête
    /// relayée (si elle l'a été) et la réponse.
    async fn api_servie(&mut self, jeton: &str) -> (Option<Value>, reqwest::Response) {
        let envoi = {
            let r = self
                .http
                .get(format!("{}/api/relay/{SID}/library/albums", self.base))
                .header("x-bridge-token", jeton);
            tokio::spawn(async move { r.send().await.unwrap() })
        };
        let relayee = tokio::select! {
            t = self.serveur.recv() => t.map(|t| serde_json::from_str::<Value>(&t).unwrap()),
            _ = tokio::time::sleep(Duration::from_millis(500)) => None,
        };
        if let Some(r) = &relayee {
            let rep = json!({"type": "relay.response", "id": r["id"], "status": 200,
                             "headers": {"content-type": "application/json"}, "body": "[]"});
            crate::ws_server::handle_server_message(&self.state, SID, &rep.to_string()).await;
        }
        let rep = tokio::time::timeout(PATIENCE, envoi)
            .await
            .unwrap()
            .unwrap();
        (relayee, rep)
    }

    /// Ouvre un flux audio et fait jouer le faux serveur : en-tête, puis un
    /// premier morceau. Rend la réponse, dont le corps reste ouvert.
    async fn flux(&mut self, jeton: &str) -> (reqwest::Response, String) {
        let envoi = {
            let r = self.http.get(format!(
                "{}/stream/relay/{SID}/piste.flac?token={jeton}",
                self.base
            ));
            tokio::spawn(async move { r.send().await.unwrap() })
        };
        let trame = tokio::time::timeout(PATIENCE, self.serveur.recv())
            .await
            .expect("aucune demande de flux relayee")
            .unwrap();
        let v: Value = serde_json::from_str(&trame).unwrap();
        assert_eq!(v["type"], "relay.stream_request");
        let id = v["id"].as_str().unwrap().to_string();
        let debut = json!({"type": "relay.stream_start", "id": id, "status": 200,
                           "headers": {"content-type": "audio/flac"}});
        crate::ws_server::handle_server_message(&self.state, SID, &debut.to_string()).await;
        self.morceau(&id, b"fLaC-debut").await;
        let rep = tokio::time::timeout(PATIENCE, envoi)
            .await
            .unwrap()
            .unwrap();
        (rep, id)
    }

    async fn morceau(&self, id: &str, octets: &[u8]) {
        let b64 = tune_laissez_passer::en_b64(octets);
        crate::ws_server::handle_server_message(&self.state, SID, &format!("BINARY:{id}:{b64}"))
            .await;
    }
}

/// Le corps se termine-t-il (fin ou erreur) avant `PATIENCE` ?
async fn le_corps_se_ferme(rep: &mut reqwest::Response) -> bool {
    loop {
        match tokio::time::timeout(PATIENCE, rep.chunk()).await {
            Err(_) => return false,
            Ok(Ok(Some(_))) => continue,
            Ok(Ok(None)) | Ok(Err(_)) => return true,
        }
    }
}

async fn corps(rep: reqwest::Response) -> Value {
    rep.json().await.unwrap_or(Value::Null)
}

#[tokio::test]
async fn une_session_ouverte_par_laissez_passer_passe_sur_l_api_et_annonce_l_appareil() {
    let mut b = banc(false).await;
    b.site
        .noms
        .lock()
        .unwrap()
        .push(("app-iphone".into(), "iPhone de Bertrand".into()));
    let jeton = b.ouvrir(&iphone(), "cpt", "app-iphone", "j1").await;
    assert_eq!(jeton.len(), 43, "256 bits en base64url");
    assert_ne!(jeton, JETON_HERITE);

    let (relayee, rep) = b.api_servie(&jeton).await;
    assert_eq!(rep.status(), 200);
    let relayee = relayee.expect("la requete n'a pas ete relayee");
    let entetes = &relayee["headers"];
    assert_eq!(entetes["x-tune-appareil-distant"], "app-iphone");
    assert_eq!(
        entetes["x-tune-appareil-distant-nom"],
        "iPhone%20de%20Bertrand"
    );
    assert!(
        !relayee.to_string().contains(&jeton),
        "le jeton de session a atteint le serveur : {relayee}"
    );
    // L'ouverture a été demandée au site, sans rien d'autre que ses champs.
    let o = b.site.ouvertures.lock().unwrap().clone();
    assert_eq!(o.len(), 1);
    assert_eq!(
        (
            o[0].sub.as_str(),
            o[0].did.as_str(),
            o[0].sid.as_str(),
            o[0].chemin
        ),
        ("cpt", "app-iphone", SID, "relais")
    );
}

#[tokio::test]
async fn le_jeton_de_session_est_exige_sur_l_api_le_flux_et_le_websocket() {
    let b = banc(false).await;
    let sans = b.api(None).await;
    assert_eq!(sans.status(), 401);
    let faux = b.api(Some("pas-un-jeton")).await;
    assert_eq!(faux.status(), 401);
    assert_eq!(corps(faux).await["code"], "non_autorise");

    let flux = b
        .http
        .get(format!("{}/stream/relay/{SID}/piste.flac", b.base))
        .send()
        .await
        .unwrap();
    assert_eq!(flux.status(), 401);

    let ws = tokio_tungstenite::connect_async(format!(
        "{}/ws/client/{SID}?token=pas-un-jeton",
        b.base.replace("http://", "ws://")
    ))
    .await;
    assert!(
        ws.is_err(),
        "un WebSocket sans jeton de session a ete ouvert"
    );

    // Le jeton de session ouvre le WebSocket de SON serveur.
    let jeton = b.ouvrir(&iphone(), "cpt", "app", "j1").await;
    let ws = tokio_tungstenite::connect_async(format!(
        "{}/ws/client/{SID}?token={jeton}",
        b.base.replace("http://", "ws://")
    ))
    .await;
    assert!(ws.is_ok());
    // Et rien d'autre.
    let autre = b
        .http
        .get(format!(
            "{}/api/relay/6f1c1f5e-2b0a-4c2e-9d3b-7a51c0de0002/x",
            b.base
        ))
        .header("x-bridge-token", &jeton)
        .send()
        .await
        .unwrap();
    assert_ne!(autre.status(), 200);
}

#[tokio::test]
async fn la_coupure_par_le_site_ferme_le_flux_audio_en_cours_et_le_websocket() {
    let mut b = banc(false).await;
    let jeton = b.ouvrir(&iphone(), "cpt", "app", "j1").await;
    let (mut rep, id) = b.flux(&jeton).await;
    assert_eq!(rep.status(), 200);
    assert_eq!(
        rep.chunk().await.unwrap().as_deref(),
        Some(&b"fLaC-debut"[..])
    );

    let (mut ws, _) = tokio_tungstenite::connect_async(format!(
        "{}/ws/client/{SID}?token={jeton}",
        b.base.replace("http://", "ws://")
    ))
    .await
    .expect("le WebSocket de la session doit s'ouvrir");

    // Le site ordonne la coupure au battement.
    *b.site.reponse_battement.lock().unwrap() = ReponseBattement {
        couper: vec![crate::site::Coupure {
            session_id: "s1".into(),
            motif: Some("arretee_depuis_l_espace_client".into()),
        }],
        ..Default::default()
    };
    b.state.sessions.battre().await;

    assert!(
        le_corps_se_ferme(&mut rep).await,
        "le flux audio continue apres la coupure"
    );
    // Un morceau arrivé après la coupure ne bloque rien.
    b.morceau(&id, b"apres").await;

    let fermeture = tokio::time::timeout(PATIENCE, async {
        while let Some(m) = ws.next().await {
            if let Ok(tokio_tungstenite::tungstenite::Message::Close(cadre)) = m {
                return cadre;
            }
        }
        None
    })
    .await
    .expect("le WebSocket n'a pas ete ferme");
    let cadre = fermeture.expect("fermeture sans cadre");
    assert_eq!(
        u16::from(cadre.code),
        crate::ws_client::FERMETURE_SESSION_COUPEE
    );
    assert!(cadre.reason.contains("arretee_depuis_l_espace_client"));

    let apres = b.api(Some(&jeton)).await;
    assert_eq!(apres.status(), 401);
    let c = corps(apres).await;
    assert_eq!(c["code"], "session_coupee");
    assert_eq!(c["motif"], "arretee_depuis_l_espace_client");
}

#[tokio::test]
async fn une_seule_ecoute_par_compte_la_nouvelle_coupe_l_ancienne() {
    let mut b = banc(false).await;
    b.site
        .noms
        .lock()
        .unwrap()
        .push(("app-ipad".into(), "iPad".into()));
    let a = b.ouvrir(&iphone(), "cpt", "app-iphone", "j1").await;
    let c = b.ouvrir(&ipad(), "cpt", "app-ipad", "j2").await;
    let (mut flux_a, _) = b.flux(&a).await;
    assert_eq!(flux_a.status(), 200);
    assert!(flux_a.chunk().await.unwrap().is_some());

    // Parcourir la bibliothèque sur l'iPad ne compte pas comme une écoute.
    let (_, rep) = b.api_servie(&c).await;
    assert_eq!(rep.status(), 200);
    let (_, rep) = b.api_servie(&a).await;
    assert_eq!(rep.status(), 200, "parcourir ne doit pas couper l'ecoute");

    // L'iPad lance l'audio : l'iPhone s'arrête.
    let (flux_c, _) = b.flux(&c).await;
    assert_eq!(flux_c.status(), 200);
    assert!(
        le_corps_se_ferme(&mut flux_a).await,
        "l'ancienne ecoute continue"
    );
    let rep = b.api(Some(&a)).await;
    assert_eq!(rep.status(), 401);
    let v = corps(rep).await;
    assert_eq!(v["code"], "session_coupee");
    assert_eq!(v["motif"], "ecoute_reprise_ailleurs");
    assert_eq!(v["appareil"], "iPad");

    // Un autre compte n'est pas touché, et ne touche pas.
    let autre = b.ouvrir(&iphone(), "cpt-autre", "app-x", "j3").await;
    let (rep, _) = b.flux(&autre).await;
    assert_eq!(rep.status(), 200);
    let (_, rep) = b.api_servie(&c).await;
    assert_eq!(
        rep.status(),
        200,
        "l'ecoute d'un autre compte a coupe celle-ci"
    );
}

#[tokio::test]
async fn le_renouvellement_garde_le_jeton_de_session() {
    let mut b = banc(false).await;
    let jeton = b.ouvrir(&iphone(), "cpt", "app", "j1").await;
    b.avancer(45 * 60);
    let lp = laissez_passer(&iphone(), "cpt", "app", b.maintenant(), "j2");
    let defi = b.defi().await;
    let r = b
        .http
        .post(format!("{}/session/renouveler", b.base))
        .header("x-bridge-token", &jeton)
        .json(&json!({"laissez_passer": lp, "defi": defi,
                      "signature": iphone().signer_defi(SID, &defi)}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["jeton"], jeton.as_str());
    assert_eq!(v["expire"], b.maintenant() + 3600);
    // Au-delà de l'ancienne fin, la session vit encore.
    b.avancer(30 * 60);
    let (_, rep) = b.api_servie(&jeton).await;
    assert_eq!(rep.status(), 200);

    // Un laissez-passer d'un autre appareil ne renouvelle pas cette session.
    let lp = laissez_passer(&ipad(), "cpt", "app-ipad", b.maintenant(), "j3");
    let defi = b.defi().await;
    let r = b
        .http
        .post(format!("{}/session/renouveler", b.base))
        .header("x-bridge-token", &jeton)
        .json(&json!({"laissez_passer": lp, "defi": defi,
                      "signature": ipad().signer_defi(SID, &defi)}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(corps(r).await["code"], "autre_appareil");
}

#[tokio::test]
async fn sans_renouvellement_la_session_expire() {
    let b = banc(false).await;
    let jeton = b.ouvrir(&iphone(), "cpt", "app", "j1").await;
    b.avancer(3600 + 121);
    let r = b.api(Some(&jeton)).await;
    assert_eq!(r.status(), 401);
    assert_eq!(corps(r).await["code"], "session_expiree");
}

#[tokio::test]
async fn le_chemin_herite_est_accepte_avec_un_avertissement_puis_refuse_par_l_interrupteur() {
    let mut ouvert = banc(true).await;
    let (relayee, rep) = ouvert.api_servie(JETON_HERITE).await;
    assert!(relayee.is_some());
    assert_eq!(rep.status(), 200);
    assert_eq!(
        rep.headers()
            .get("x-tune-avertissement")
            .and_then(|v| v.to_str().ok()),
        Some("lien-obsolete")
    );
    // Compté à part, par serveur, et rendu au site au battement.
    ouvert.state.sessions.battre().await;
    let battements = ouvert.site.battements.lock().unwrap().clone();
    assert_eq!(battements.len(), 1);
    assert_eq!(battements[0]["jeton_herite"][SID], 1);

    let ferme = banc(false).await;
    let rep = ferme.api(Some(JETON_HERITE)).await;
    assert_eq!(rep.status(), 401);
    assert_eq!(corps(rep).await["code"], "lien_obsolete");
    let flux = ferme
        .http
        .get(format!(
            "{}/stream/relay/{SID}/piste.flac?token={JETON_HERITE}",
            ferme.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(flux.status(), 401);
    let ws = tokio_tungstenite::connect_async(format!(
        "{}/ws/client/{SID}?token={JETON_HERITE}",
        ferme.base.replace("http://", "ws://")
    ))
    .await;
    assert!(ws.is_err());
}

#[tokio::test]
async fn site_en_panne_un_laissez_passer_valide_passe_et_l_ouverture_est_rejouee() {
    let b = banc(false).await;
    b.site.panne(true);
    let lp = laissez_passer(&iphone(), "cpt", "app", b.maintenant(), "j1");
    let (statut, v) = b.ouvrir_avec(&iphone(), &lp).await;
    assert_eq!(statut, 200, "{v}");
    assert_eq!(v["secours"], false);
    assert!(b.site.ouvertures.lock().unwrap().is_empty());

    b.site.panne(false);
    b.state.sessions.battre().await;
    let o = b.site.ouvertures.lock().unwrap().clone();
    assert_eq!(o.len(), 1, "l'ouverture n'a pas ete rejouee");
    assert_eq!(o[0].jti, "j1");
}

#[tokio::test]
async fn site_en_panne_un_laissez_passer_expire_ne_passe_que_pour_un_appareil_connu() {
    let b = banc(false).await;
    // L'iPhone s'est déjà connecté, site joignable : il est connu.
    b.ouvrir(&iphone(), "cpt", "app-iphone", "j0").await;

    let lp_iphone = laissez_passer(&iphone(), "cpt", "app-iphone", b.maintenant(), "j1");
    let lp_inconnu = laissez_passer(&ipad(), "cpt", "app-ipad", b.maintenant(), "j2");
    b.avancer(3 * 3600);

    // Site joignable : expiré, c'est expiré.
    let (statut, v) = b.ouvrir_avec(&iphone(), &lp_iphone).await;
    assert_eq!((statut, v["code"].as_str()), (401, Some("expire")));

    // Site en panne, sans battement pour s'en apercevoir : le relais sonde.
    b.site.panne(true);
    assert!(!b.state.sessions.site_injoignable());

    let (statut, v) = b.ouvrir_avec(&iphone(), &lp_iphone).await;
    assert_eq!(statut, 200, "{v}");
    assert_eq!(v["secours"], true);

    let (statut, v) = b.ouvrir_avec(&ipad(), &lp_inconnu).await;
    assert_eq!(
        (statut, v["code"].as_str()),
        (503, Some("cloud_unavailable"))
    );

    // Au-delà de 24 h, plus rien.
    b.avancer(24 * 3600);
    let (statut, v) = b.ouvrir_avec(&iphone(), &lp_iphone).await;
    assert_eq!((statut, v["code"].as_str()), (401, Some("expire")));
}

#[tokio::test]
async fn la_revocation_au_battement_coupe_la_session_et_refuse_la_reouverture() {
    let mut b = banc(false).await;
    let jeton = b.ouvrir(&iphone(), "cpt", "app", "j1").await;
    let (mut flux, _) = b.flux(&jeton).await;
    *b.site.reponse_battement.lock().unwrap() = ReponseBattement {
        revoques: crate::site::Revoques {
            jti: vec![],
            did: vec!["app".into()],
        },
        ..Default::default()
    };
    b.state.sessions.battre().await;
    assert!(le_corps_se_ferme(&mut flux).await);
    let r = b.api(Some(&jeton)).await;
    assert_eq!(corps(r).await["motif"], "revoquee");

    let lp = laissez_passer(&iphone(), "cpt", "app", b.maintenant(), "j2");
    let (statut, v) = b.ouvrir_avec(&iphone(), &lp).await;
    assert_eq!((statut, v["code"].as_str()), (403, Some("revoque")));
}

#[tokio::test]
async fn le_site_peut_refuser_l_ouverture() {
    let b = banc(false).await;
    *b.site.refus.lock().unwrap() = Some("pas_premium".into());
    let lp = laissez_passer(&iphone(), "cpt", "app", b.maintenant(), "j1");
    let (statut, v) = b.ouvrir_avec(&iphone(), &lp).await;
    assert_eq!(statut, 403);
    assert_eq!(v["code"], "refuse_par_le_site");
    assert_eq!(v["motif"], "pas_premium");
}

#[tokio::test]
async fn le_defi_ne_sert_qu_une_fois_pour_son_serveur_et_pas_au_dela_d_une_minute() {
    let b = banc(false).await;
    let lp = laissez_passer(&iphone(), "cpt", "app", b.maintenant(), "j1");
    let ouvrir = |defi: String, lp: String| {
        let http = b.http.clone();
        let url = format!("{}/session/{SID}", b.base);
        async move {
            let r = http
                .post(url)
                .json(&json!({"laissez_passer": lp, "defi": defi,
                              "signature": iphone().signer_defi(SID, &defi)}))
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap())
        }
    };
    let defi = b.defi().await;
    assert_eq!(ouvrir(defi.clone(), lp.clone()).await.0, 200);
    let (statut, v) = ouvrir(defi, lp.clone()).await;
    assert_eq!((statut, v["code"].as_str()), (401, Some("defi")));

    let vieux = b.defi().await;
    b.avancer(61);
    let (statut, v) = ouvrir(vieux, lp.clone()).await;
    assert_eq!((statut, v["code"].as_str()), (401, Some("defi")));

    // Un défi inventé par l'app n'existe pas.
    let (statut, _) = ouvrir("invente".into(), lp).await;
    assert_eq!(statut, 401);

    // Pas de défi pour un serveur absent.
    let r = b
        .http
        .get(format!("{}/session/defi/inconnu", b.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn le_laissez_passer_d_une_autre_cle_ou_d_un_autre_serveur_est_refuse() {
    let b = banc(false).await;
    // Laissez-passer de l'iPhone, défi signé par l'iPad.
    let lp = laissez_passer(&iphone(), "cpt", "app", b.maintenant(), "j1");
    let (statut, v) = b.ouvrir_avec(&ipad(), &lp).await;
    assert_eq!((statut, v["code"].as_str()), (401, Some("defi")));

    // Laissez-passer d'un autre serveur.
    let autre = cles_de_test::site().signer(&revendications(
        "cpt",
        "6f1c1f5e-2b0a-4c2e-9d3b-7a51c0de0002",
        "app",
        iphone().confirmation(),
        b.maintenant(),
        "j2",
    ));
    let (statut, v) = b.ouvrir_avec(&iphone(), &autre).await;
    assert_eq!((statut, v["code"].as_str()), (401, Some("serveur")));
    assert!(b.site.ouvertures.lock().unwrap().is_empty());
}

#[tokio::test]
async fn sans_racine_rien_ne_passe() {
    let b = banc_avec(false, "", false).await;
    assert!(b.state.sessions.rafraichir_jeu().await.is_err());
    let lp = laissez_passer(&iphone(), "cpt", "app", b.maintenant(), "j1");
    let (statut, v) = b.ouvrir_avec(&iphone(), &lp).await;
    assert_eq!(
        (statut, v["code"].as_str()),
        (503, Some("jeu_de_cles_absent"))
    );
}

#[tokio::test]
async fn un_kid_inconnu_fait_relire_le_jeu_de_cles_une_fois_par_minute_au_plus() {
    let b = banc(false).await;
    let avant = b.site.lectures_du_jeu.load(Ordering::SeqCst);
    let inconnu = tune_laissez_passer::fabrique::SignataireDuSite::depuis_graine(
        "k-2027",
        &cles_de_test::GRAINE_SITE,
    );
    let lp = inconnu.signer(&revendications(
        "cpt",
        SID,
        "app",
        iphone().confirmation(),
        b.maintenant(),
        "j1",
    ));
    let (statut, v) = b.ouvrir_avec(&iphone(), &lp).await;
    assert_eq!((statut, v["code"].as_str()), (401, Some("kid_inconnu")));
    let (statut, _) = b.ouvrir_avec(&iphone(), &lp).await;
    assert_eq!(statut, 401);
    assert_eq!(b.site.lectures_du_jeu.load(Ordering::SeqCst), avant + 1);

    // Le site publie la nouvelle clé : une minute plus tard, elle est prise.
    let mut doc = cles_de_test::document(2);
    let mut cle = doc.cles[0].clone();
    cle.kid = "k-2027".into();
    doc.cles.push(cle);
    *b.site.publication.lock().unwrap() = Some(cles_de_test::racine().publier(&doc));
    b.avancer(60);
    let lp = inconnu.signer(&revendications(
        "cpt",
        SID,
        "app",
        iphone().confirmation(),
        b.maintenant(),
        "j2",
    ));
    let (statut, v) = b.ouvrir_avec(&iphone(), &lp).await;
    assert_eq!(statut, 200, "{v}");
}

#[tokio::test]
async fn un_jeu_de_cles_plus_ancien_ou_d_une_autre_racine_est_refuse() {
    let b = banc(false).await;
    *b.site.publication.lock().unwrap() =
        Some(cles_de_test::racine().publier(&cles_de_test::document(5)));
    assert_eq!(b.state.sessions.rafraichir_jeu().await, Ok(5));
    *b.site.publication.lock().unwrap() =
        Some(cles_de_test::racine().publier(&cles_de_test::document(4)));
    assert!(b.state.sessions.rafraichir_jeu().await.is_err());
    let autre = tune_laissez_passer::fabrique::Racine::depuis_graine(
        *b"autreRAC",
        &cles_de_test::GRAINE_AUTRE_RACINE,
    );
    *b.site.publication.lock().unwrap() = Some(autre.publier(&cles_de_test::document(9)));
    assert!(b.state.sessions.rafraichir_jeu().await.is_err());
}

#[tokio::test]
async fn le_battement_rend_compte_des_octets_et_de_l_audio_sans_rien_d_autre() {
    let mut b = banc(false).await;
    let jeton = b.ouvrir(&iphone(), "cpt", "app", "j1").await;
    let (mut flux, _) = b.flux(&jeton).await;
    assert!(flux.chunk().await.unwrap().is_some());
    b.state.sessions.battre().await;
    let battements = b.site.battements.lock().unwrap().clone();
    assert_eq!(battements.len(), 1);
    let s = &battements[0]["sessions"][0];
    assert_eq!(s["session_id"], "s1");
    assert_eq!(s["audio_actif"], true);
    assert!(s["octets_depuis_le_dernier"].as_u64().unwrap() >= 10);
    let champs: Vec<&String> = s.as_object().unwrap().keys().collect();
    assert_eq!(
        champs,
        ["audio_actif", "octets_depuis_le_dernier", "session_id"]
    );

    // Deux minutes sans audio : la session ne compte plus comme écoute.
    b.avancer(121);
    b.state.sessions.battre().await;
    let battements = b.site.battements.lock().unwrap().clone();
    assert_eq!(battements[1]["sessions"][0]["audio_actif"], false);
    assert_eq!(battements[1]["sessions"][0]["octets_depuis_le_dernier"], 0);
}

#[tokio::test]
async fn un_battement_perdu_ne_perd_pas_les_compteurs() {
    let mut b = banc(false).await;
    let jeton = b.ouvrir(&iphone(), "cpt", "app", "j1").await;
    let (mut flux, _) = b.flux(&jeton).await;
    assert!(flux.chunk().await.unwrap().is_some());
    b.site.panne(true);
    b.state.sessions.battre().await;
    b.site.panne(false);
    b.state.sessions.battre().await;
    let battements = b.site.battements.lock().unwrap().clone();
    assert_eq!(battements.len(), 1);
    assert!(
        battements[0]["sessions"][0]["octets_depuis_le_dernier"]
            .as_u64()
            .unwrap()
            >= 10
    );
}

#[tokio::test]
async fn le_serveur_s_enregistre_en_signant_le_defi_avec_sa_cle() {
    let b = banc(false).await;
    let cle =
        tune_laissez_passer::CleServeur::depuis_graine(&cles_de_test::GRAINE_SERVEUR).unwrap();
    let pk = cle.cle_publique();
    let sid = "srv-signe";

    // Bonne signature : la clé est prouvée.
    let prouvee = crate::ws_server::authentifier_serveur(&b.state, sid, Some(&pk), |defi| {
        let sig = cle.signer_defi(sid, &defi);
        async move { Some(sig) }
    })
    .await;
    assert_eq!(prouvee, Ok(Some(pk.clone())));

    // Signature d'un autre server_id : refusée.
    let r = crate::ws_server::authentifier_serveur(&b.state, sid, Some(&pk), |defi| {
        let sig = cle.signer_defi("autre", &defi);
        async move { Some(sig) }
    })
    .await;
    assert_eq!(r, Err("signature du serveur invalide"));

    // Pas de réponse au défi : refusé.
    let r =
        crate::ws_server::authentifier_serveur(&b.state, sid, Some(&pk), |_| async { None }).await;
    assert_eq!(r, Err("defi du serveur sans reponse"));

    // Le site connaît la clé de ce serveur : sans la prouver, pas d'entrée.
    *b.site.cle_serveur.lock().unwrap() = Some(pk.clone());
    let r = crate::ws_server::authentifier_serveur(&b.state, sid, None, |_| async { None }).await;
    assert_eq!(r, Err("cle du serveur requise"));
    // Site en panne : la dernière clé connue tient.
    b.site.panne(true);
    let r = crate::ws_server::authentifier_serveur(&b.state, sid, None, |_| async { None }).await;
    assert_eq!(r, Err("cle du serveur requise"));
    b.site.panne(false);

    // Un serveur vivant enregistré avec sa clé n'est remplacé que par sa clé,
    // même par quelqu'un qui présente le bon jeton du pont.
    let tx = mpsc::channel::<String>(1).0;
    b.state
        .register_server(
            sid.into(),
            "S".into(),
            "tok-s".into(),
            Some(pk.clone()),
            tx.clone(),
        )
        .unwrap();
    assert_eq!(
        b.state
            .register_server(sid.into(), "S".into(), "tok-s".into(), None, tx.clone()),
        Err("server_id already registered")
    );
    assert!(
        b.state
            .register_server(sid.into(), "S".into(), "tok-s".into(), Some(pk), tx)
            .is_ok()
    );
}

#[tokio::test]
async fn un_laissez_passer_qui_nomme_une_autre_cle_de_serveur_est_refuse() {
    let b = banc(false).await;
    let cle =
        tune_laissez_passer::CleServeur::depuis_graine(&cles_de_test::GRAINE_SERVEUR).unwrap();
    let tx = mpsc::channel::<String>(1).0;
    b.state.unregister_server(SID);
    b.state
        .register_server(
            SID.into(),
            "Salon".into(),
            JETON_HERITE.into(),
            Some(cle.cle_publique()),
            tx,
        )
        .unwrap();
    let mut r = revendications(
        "cpt",
        SID,
        "app",
        iphone().confirmation(),
        b.maintenant(),
        "j1",
    );
    r.srv_pk = Some(tune_laissez_passer::en_b64url(&[9u8; 32]));
    let (statut, v) = b
        .ouvrir_avec(&iphone(), &cles_de_test::site().signer(&r))
        .await;
    assert_eq!((statut, v["code"].as_str()), (401, Some("serveur")));
    r.srv_pk = Some(cle.cle_publique());
    let (statut, _) = b
        .ouvrir_avec(&iphone(), &cles_de_test::site().signer(&r))
        .await;
    assert_eq!(statut, 200);
}

/// Un écrivain de traces en mémoire.
#[derive(Clone, Default)]
struct Traces(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Traces {
    fn write(&mut self, octets: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(octets);
        Ok(octets.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Traces {
    type Writer = Traces;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn aucun_jeton_ni_laissez_passer_dans_les_traces() {
    use tracing::dispatcher::{Dispatch, set_default};
    use tracing::subscriber::NoSubscriber;

    let traces = Traces::default();
    // Un abonne LOCAL a ce test (`set_default` est propre au fil), avec son
    // ecrivain en memoire. Mais le registre des points d'appel de `tracing`
    // ne compte que les abonnes vivants du PROCESSUS : s'il n'y en a qu'un,
    // il calcule l'interet d'un point d'appel avec l'abonne du fil qui
    // l'enregistre en premier. Un autre test, qui n'a aucun abonne, y
    // inscrirait « jamais » pour toute la suite, et la trace de ce test
    // disparaissait (ex. `ecoute_distante_session_coupee`), une fois sur
    // deux sous charge. Un second abonne vivant, ici muet, force le mode
    // « tous les abonnes », ou notre abonne est toujours consulte.
    let _veille = Dispatch::new(NoSubscriber::default());
    let abonne = Dispatch::new(
        tracing_subscriber::fmt()
            .with_writer(traces.clone())
            .with_max_level(tracing::Level::TRACE)
            .finish(),
    );
    let _garde = set_default(&abonne);

    let mut b = banc(true).await;
    // Un autre test, parti en meme temps, a pu inscrire un point d'appel dans
    // la fenetre ou notre abonne n'etait pas encore au registre : creer un
    // abonne recalcule l'interet de tous les points d'appel.
    drop(Dispatch::new(NoSubscriber::default()));
    let lp = laissez_passer(&iphone(), "cpt", "app", b.maintenant(), "j1");
    let (statut, v) = b.ouvrir_avec(&iphone(), &lp).await;
    assert_eq!(statut, 200);
    let jeton = v["jeton"].as_str().unwrap().to_string();
    let (_, rep) = b.api_servie(&jeton).await;
    assert_eq!(rep.status(), 200);
    let (_, rep) = b.api_servie(JETON_HERITE).await;
    assert_eq!(rep.status(), 200);
    let (mut flux, _) = b.flux(&jeton).await;
    assert!(flux.chunk().await.unwrap().is_some());
    *b.site.reponse_battement.lock().unwrap() = ReponseBattement {
        couper: vec![crate::site::Coupure {
            session_id: "s1".into(),
            motif: None,
        }],
        ..Default::default()
    };
    b.state.sessions.battre().await;
    b.site.panne(true);
    let lp2 = laissez_passer(&iphone(), "cpt", "app", b.maintenant(), "j2");
    let (statut, v2) = b.ouvrir_avec(&iphone(), &lp2).await;
    assert_eq!(statut, 200);
    let jeton2 = v2["jeton"].as_str().unwrap().to_string();

    let ecrit = String::from_utf8(traces.0.lock().unwrap().clone()).unwrap();
    assert!(
        ecrit.contains("ecoute_distante_session_coupee"),
        "les traces n'ont pas ete capturees : {ecrit}"
    );
    for secret in [
        jeton.as_str(),
        jeton2.as_str(),
        JETON_HERITE,
        lp.as_str(),
        lp2.as_str(),
    ] {
        assert!(!ecrit.contains(secret), "un secret est dans les traces");
    }
    // Ni la seule signature d'un laissez-passer.
    let signature = lp.rsplit('.').next().unwrap();
    assert!(!ecrit.contains(signature));
}

#[tokio::test]
async fn le_relais_n_annonce_le_laissez_passer_qu_avec_un_jeu_de_cles() {
    assert!(banc(false).await.state.sessions.actif());
    assert!(!banc_avec(false, "", false).await.state.sessions.actif());
}

#[tokio::test]
async fn le_site_revenu_un_laissez_passer_expire_ne_passe_plus_en_secours() {
    let b = banc(false).await;
    // L'iPhone est connu ; son laissez-passer a expiré.
    b.ouvrir(&iphone(), "cpt", "app-iphone", "j0").await;
    let lp = laissez_passer(&iphone(), "cpt", "app-iphone", b.maintenant(), "j1");
    b.avancer(3 * 3600);

    // Panne constatée par le relais, puis retour du site, sans battement ni
    // ouverture réussie entre les deux.
    b.site.panne(true);
    let (statut, _) = b.ouvrir_avec(&iphone(), &lp).await;
    assert_eq!(statut, 200, "en panne, le secours doit jouer");
    assert!(b.state.sessions.site_injoignable());
    b.site.panne(false);

    let (statut, v) = b.ouvrir_avec(&iphone(), &lp).await;
    assert_eq!(
        (statut, v["code"].as_str()),
        (401, Some("expire")),
        "site revenu : le secours ne doit plus jouer ({v})"
    );
    assert!(!b.state.sessions.site_injoignable());
}
