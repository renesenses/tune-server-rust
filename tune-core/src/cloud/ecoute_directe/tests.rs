//! Écoute directe (#6785) : de bout en bout, deux endpoints iroh dans le
//! même processus sur 127.0.0.1, sans relais (`RelayMode::Disabled`), sans
//! portmapper ni découverte ; un faux serveur local (axum) et un faux site.
//! Aucun réseau extérieur.

use std::sync::Mutex as StdMutex;

use axum::Router;
use axum::body::Body;
use axum::http::HeaderMap;
use axum::routing::get;
use tokio::io::AsyncReadExt;
use tune_laissez_passer::fabrique::cles_de_test::{self, *};
use tune_laissez_passer::fabrique::{AppareilDeTest, revendications};
use tune_laissez_passer::{Revendications, en_b64url, message_defi_direct};

use super::*;
use crate::db::migrations;
use crate::db::sqlite::SqliteDb;

const SID: &str = "srv-direct-1";
const PATIENCE: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Faux site, faux droits, faux serveur local
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FauxSite {
    ouvertures: StdMutex<Vec<DemandeOuverture>>,
    battements: StdMutex<Vec<Battement>>,
    /// `Some(motif)` : refuse les ouvertures.
    refuse: StdMutex<Option<String>>,
    en_panne: AtomicBool,
    /// Sessions à couper, rendues par l'ouverture suivante.
    couper_a_l_ouverture: StdMutex<Vec<String>>,
    reponse_battement: StdMutex<ReponseBattement>,
    compteur: AtomicU64,
}

#[async_trait]
impl SiteDirect for FauxSite {
    async fn ouvrir_session(
        &self,
        d: &DemandeOuverture,
    ) -> Result<ReponseOuverture, SiteInjoignable> {
        if self.en_panne.load(Ordering::Relaxed) {
            return Err(SiteInjoignable("panne".into()));
        }
        self.ouvertures.lock().unwrap().push(d.clone());
        if let Some(m) = self.refuse.lock().unwrap().clone() {
            return Ok(ReponseOuverture {
                ok: false,
                motif: Some(m),
                ..Default::default()
            });
        }
        let n = self.compteur.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(ReponseOuverture {
            ok: true,
            session_id: Some(format!("site-{n}")),
            couper: std::mem::take(&mut *self.couper_a_l_ouverture.lock().unwrap()),
            motif: None,
            appareil: Some(format!("Appareil {n}")),
        })
    }

    async fn battement(&self, b: &Battement) -> Result<ReponseBattement, SiteInjoignable> {
        if self.en_panne.load(Ordering::Relaxed) {
            return Err(SiteInjoignable("panne".into()));
        }
        self.battements.lock().unwrap().push(b.clone());
        Ok(self.reponse_battement.lock().unwrap().clone())
    }

    async fn jeu_de_cles(&self) -> Result<Publication, SiteInjoignable> {
        Ok(cles_de_test::publication())
    }
}

struct FauxDroits(AtomicBool);

#[async_trait]
impl DroitsDirect for FauxDroits {
    async fn ecoute_distante_permise(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Chemins reçus par le faux serveur local.
type Journal = Arc<StdMutex<Vec<String>>>;

async fn faux_serveur_local(journal: Journal) -> u16 {
    let j1 = journal.clone();
    let j2 = journal.clone();
    let j3 = journal.clone();
    let app = Router::new()
        .route(
            "/api/v1/echo",
            get(move |h: HeaderMap| {
                let j = j1.clone();
                async move {
                    j.lock().unwrap().push("/api/v1/echo".into());
                    let v = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(String::from);
                    axum::Json(serde_json::json!({
                        "relais": v("x-tune-relais"),
                        "appareil": v("x-tune-appareil-distant"),
                        "appareil_nom": v("x-tune-appareil-distant-nom"),
                        "cookie": v("cookie"),
                        "profil": v("x-profile-id"),
                    }))
                }
            }),
        )
        .route(
            "/stream/{*p}",
            get(move || {
                let j = j2.clone();
                async move {
                    j.lock().unwrap().push("/stream".into());
                    // Un flux sans fin : 1 Kio toutes les 20 ms.
                    let s = futures_util::stream::unfold((), |()| async {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Some((Ok::<_, std::io::Error>(Bytes::from(vec![0x5a; 1024])), ()))
                    });
                    Body::from_stream(s)
                }
            }),
        )
        .route(
            "/autre",
            get(move || {
                let j = j3.clone();
                async move {
                    j.lock().unwrap().push("/autre".into());
                    "secret"
                }
            }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(ecoute, app).await });
    port
}

use bytes::Bytes;

struct Banc {
    moteur: Arc<EcouteDirecte>,
    site: Arc<FauxSite>,
    droits: Arc<FauxDroits>,
    journal: Journal,
    cible: iroh::EndpointAddr,
    serveur: iroh::Endpoint,
}

impl Drop for Banc {
    fn drop(&mut self) {
        let ep = self.serveur.clone();
        tokio::spawn(async move { ep.close().await });
    }
}

async fn banc() -> Banc {
    let journal: Journal = Default::default();
    let port = faux_serveur_local(journal.clone()).await;
    let site = Arc::new(FauxSite::default());
    let droits = Arc::new(FauxDroits(AtomicBool::new(true)));
    let moteur = EcouteDirecte::nouveau(Parametres {
        server_id: SID.into(),
        graine: GRAINE_SERVEUR,
        racine: racine().cle_publique_minisign(),
        jeu_initial: Some(cles_de_test::publication()),
        site: site.clone(),
        droits: droits.clone(),
        local_port: port,
    })
    .unwrap();
    let serveur = ouvrir_endpoint(
        &GRAINE_SERVEUR,
        &ConfigReseau {
            boucle_locale: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let cible = iroh::EndpointAddr::from_parts(
        serveur.id(),
        serveur
            .bound_sockets()
            .into_iter()
            .map(iroh::TransportAddr::Ip),
    );
    tokio::spawn(moteur.clone().servir(serveur.clone()));
    Banc {
        moteur,
        site,
        droits,
        journal,
        cible,
        serveur,
    }
}

async fn client() -> iroh::Endpoint {
    use iroh::endpoint::{PortmapperConfig, presets};
    iroh::Endpoint::builder(presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr("127.0.0.1:0")
        .unwrap()
        .portmapper_config(PortmapperConfig::Disabled)
        .bind()
        .await
        .unwrap()
}

fn appareil() -> AppareilDeTest {
    AppareilDeTest::ed25519(&GRAINE_APPAREIL)
}

fn cle_publique_serveur() -> String {
    tune_laissez_passer::CleServeur::depuis_graine(&GRAINE_SERVEUR)
        .unwrap()
        .cle_publique()
}

/// Un laissez-passer direct valide : `p2p`, `srv_pk` de ce serveur.
fn rev(jti: &str) -> Revendications {
    let mut r = revendications(
        "compte-1",
        SID,
        "appareil-1",
        appareil().confirmation(),
        maintenant() - 60,
        jti,
    );
    r.p2p = true;
    r.srv_pk = Some(cle_publique_serveur());
    r
}

fn signer(r: &Revendications) -> String {
    site().signer(r)
}

/// Une session ouverte côté app : la connexion, le flux de contrôle, et la
/// réponse du serveur à `ouvrir`.
struct CoteApp {
    _ep: iroh::Endpoint,
    conn: iroh::endpoint::Connection,
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
    reponse: MessageServeur,
}

/// Compose le serveur et présente `jeton`, en signant le défi avec `signe`.
async fn ouvrir_avec(b: &Banc, jeton: &str, signe: impl Fn(&str, &str) -> String) -> CoteApp {
    let ep = client().await;
    let conn = tokio::time::timeout(PATIENCE, ep.connect(b.cible.clone(), ALPN))
        .await
        .expect("delai de connexion")
        .expect("connexion");
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    ecrire_trame(&mut send, &MessageApp::Bonjour { v: VERSION })
        .await
        .unwrap();
    let MessageServeur::Defi {
        server_id, defi, ..
    } = lire_trame::<_, MessageServeur>(&mut recv).await.unwrap()
    else {
        panic!("pas de defi");
    };
    assert_eq!(server_id, SID);
    let signature = signe(&server_id, &defi);
    ecrire_trame(
        &mut send,
        &MessageApp::Ouvrir {
            laissez_passer: jeton.to_string(),
            signature,
        },
    )
    .await
    .unwrap();
    let reponse = tokio::time::timeout(PATIENCE, lire_trame::<_, MessageServeur>(&mut recv))
        .await
        .expect("delai de reponse")
        .expect("reponse");
    CoteApp {
        _ep: ep,
        conn,
        send,
        recv,
        reponse,
    }
}

async fn ouvrir(b: &Banc, jeton: &str) -> CoteApp {
    ouvrir_avec(b, jeton, |sid, defi| {
        appareil().signer(&message_defi_direct(sid, defi))
    })
    .await
}

fn code_de_refus(m: &MessageServeur) -> String {
    match m {
        MessageServeur::Refus { code, .. } => code.clone(),
        autre => panic!("attendu un refus, obtenu {autre:?}"),
    }
}

/// Une requête HTTP/1.1 sur un nouveau flux de la session ; rend l'en-tête de
/// la réponse et au plus `max` octets de corps.
async fn http(conn: &iroh::endpoint::Connection, ligne: &str, max: usize) -> (String, Vec<u8>) {
    let (mut s, mut r) = conn.open_bi().await.unwrap();
    let requete = format!(
        "{ligne} HTTP/1.1\r\nHost: tune\r\nCookie: secret=1\r\nX-Profile-Id: 7\r\nConnection: close\r\n\r\n"
    );
    s.write_all(requete.as_bytes()).await.unwrap();
    let mut tout = Vec::new();
    let mut tampon = [0u8; 4096];
    let lire = async {
        loop {
            match AsyncReadExt::read(&mut r, &mut tampon).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tout.extend_from_slice(&tampon[..n]);
                    if let Some(i) = find(&tout, b"\r\n\r\n")
                        && tout.len() - (i + 4) >= max
                    {
                        break;
                    }
                }
            }
        }
    };
    let _ = tokio::time::timeout(PATIENCE, lire).await;
    let i = find(&tout, b"\r\n\r\n").expect("en-tete de reponse");
    let entete = String::from_utf8_lossy(&tout[..i]).to_string();
    (entete, tout[i + 4..].to_vec())
}

fn find(h: &[u8], aiguille: &[u8]) -> Option<usize> {
    h.windows(aiguille.len()).position(|w| w == aiguille)
}

/// Le corps JSON d'une réponse (non chunkée par axum pour un petit JSON ;
/// chunkée, on retire la taille de tête).
fn corps_json(corps: &[u8]) -> serde_json::Value {
    let texte = String::from_utf8_lossy(corps);
    let debut = texte.find('{').expect("json");
    let fin = texte.rfind('}').expect("json");
    serde_json::from_str(&texte[debut..=fin]).unwrap()
}

// ---------------------------------------------------------------------------
// Refus
// ---------------------------------------------------------------------------

#[tokio::test]
async fn un_laissez_passer_sans_p2p_est_refuse() {
    let b = banc().await;
    let mut r = rev("j-sans-p2p");
    r.p2p = false;
    let app = ouvrir(&b, &signer(&r)).await;
    assert_eq!(code_de_refus(&app.reponse), "p2p_non_permis");
    assert!(b.moteur.sessions().is_empty());
    assert!(
        b.site.ouvertures.lock().unwrap().is_empty(),
        "le site ne doit pas etre sollicite"
    );
}

#[tokio::test]
async fn le_laissez_passer_d_un_autre_serveur_est_refuse() {
    let b = banc().await;
    let mut r = rev("j-autre-sid");
    r.sid = "srv-d-un-autre".into();
    let app = ouvrir(&b, &signer(&r)).await;
    assert_eq!(code_de_refus(&app.reponse), "serveur");
    // `srv_pk` d'un autre serveur, `sid` juste : refusé aussi.
    let mut r = rev("j-autre-pk");
    r.srv_pk = Some(en_b64url(&[7u8; 32]));
    let app = ouvrir(&b, &signer(&r)).await;
    assert_eq!(code_de_refus(&app.reponse), "serveur");
    assert!(b.moteur.sessions().is_empty());
}

#[tokio::test]
async fn un_laissez_passer_expire_est_refuse_meme_site_injoignable() {
    let b = banc().await;
    let mut r = rev("j-expire");
    r.iat = maintenant() - 2 * 3600;
    r.nbf = r.iat;
    r.exp = r.iat + 3600;
    let app = ouvrir(&b, &signer(&r)).await;
    assert_eq!(code_de_refus(&app.reponse), "expire");
    b.site.en_panne.store(true, Ordering::Relaxed);
    let app = ouvrir(&b, &signer(&r)).await;
    assert_eq!(code_de_refus(&app.reponse), "expire");
}

#[tokio::test]
async fn un_laissez_passer_ou_un_appareil_revoque_est_refuse() {
    let b = banc().await;
    b.moteur.revoquer(&["j-revoque".into()], &[]);
    let app = ouvrir(&b, &signer(&rev("j-revoque"))).await;
    assert_eq!(code_de_refus(&app.reponse), "revoque");
    b.moteur.revoquer(&[], &["appareil-1".into()]);
    let app = ouvrir(&b, &signer(&rev("j-neuf"))).await;
    assert_eq!(code_de_refus(&app.reponse), "revoque");
    assert!(b.moteur.sessions().is_empty());
}

#[tokio::test]
async fn un_defi_mal_signe_est_refuse() {
    let b = banc().await;
    let jeton = signer(&rev("j-defi"));
    // Une autre clé que celle du laissez-passer.
    let app = ouvrir_avec(&b, &jeton, |sid, defi| {
        AppareilDeTest::ed25519(&GRAINE_AUTRE_APPAREIL).signer(&message_defi_direct(sid, defi))
    })
    .await;
    assert_eq!(code_de_refus(&app.reponse), "defi");
    // La bonne clé, mais le domaine du relais.
    let app = ouvrir_avec(&b, &jeton, |sid, defi| appareil().signer_defi(sid, defi)).await;
    assert_eq!(code_de_refus(&app.reponse), "defi");
    // La bonne clé, un autre défi.
    let app = ouvrir_avec(&b, &jeton, |sid, _| {
        appareil().signer(&message_defi_direct(sid, "un-autre-defi"))
    })
    .await;
    assert_eq!(code_de_refus(&app.reponse), "defi");
    assert!(b.moteur.sessions().is_empty());
}

#[tokio::test]
async fn sans_droit_premium_rien_ne_passe() {
    let b = banc().await;
    b.droits.0.store(false, Ordering::Relaxed);
    let app = ouvrir(&b, &signer(&rev("j-premium"))).await;
    assert_eq!(code_de_refus(&app.reponse), "premium_requis");
}

#[tokio::test]
async fn sans_jeu_de_cles_rien_ne_passe() {
    let journal: Journal = Default::default();
    let port = faux_serveur_local(journal).await;
    let moteur = EcouteDirecte::nouveau(Parametres {
        server_id: SID.into(),
        graine: GRAINE_SERVEUR,
        // Le jeu de test n'est pas signé par la racine de production.
        racine: tune_laissez_passer::RACINE_EMBARQUEE.into(),
        jeu_initial: Some(cles_de_test::publication()),
        site: Arc::new(FauxSite::default()),
        droits: Arc::new(FauxDroits(AtomicBool::new(true))),
        local_port: port,
    })
    .unwrap();
    let r = rev("j-sans-jeu");
    let err = moteur
        .admettre("d", &signer(&r), "s")
        .await
        .err()
        .expect("refus");
    // Le faux site rend le jeu de test, que la racine embarquée refuse
    // aussi : `kid` toujours inconnu.
    assert!(
        err.code == "jeu_de_cles_absent" || err.code == "kid_inconnu",
        "{err:?}"
    );
}

#[tokio::test]
async fn le_refus_du_site_est_rendu_a_l_app() {
    let b = banc().await;
    *b.site.refuse.lock().unwrap() = Some("compte_bloque".into());
    let app = ouvrir(&b, &signer(&rev("j-site"))).await;
    assert_eq!(
        app.reponse,
        MessageServeur::Refus {
            code: "refuse_par_le_site".into(),
            motif: Some("compte_bloque".into())
        }
    );
    assert!(b.moteur.sessions().is_empty());
}

// ---------------------------------------------------------------------------
// Session valide : le flux est servi, rien de plus
// ---------------------------------------------------------------------------

#[tokio::test]
async fn un_laissez_passer_valide_ouvre_une_session_qui_sert_l_api_et_le_flux() {
    let b = banc().await;
    let app = ouvrir(&b, &signer(&rev("j-valide"))).await;
    let MessageServeur::Ouvert { secours, .. } = &app.reponse else {
        panic!("attendu ouvert, obtenu {:?}", app.reponse);
    };
    assert!(!secours);
    let ouvertures = b.site.ouvertures.lock().unwrap().clone();
    assert_eq!(ouvertures.len(), 1);
    assert_eq!(ouvertures[0].chemin, "direct");
    assert_eq!(ouvertures[0].sid, SID);
    assert_eq!(ouvertures[0].jti, "j-valide");

    // L'API, avec les en-têtes que le relais pose, et sans le cookie.
    let (entete, corps) = http(&app.conn, "GET /api/v1/echo", usize::MAX).await;
    assert!(entete.starts_with("HTTP/1.1 200"), "{entete}");
    let v = corps_json(&corps);
    assert_eq!(v["relais"], "1");
    assert_eq!(v["appareil"], "appareil-1");
    assert_eq!(v["appareil_nom"], "Appareil 1");
    assert_eq!(v["profil"], "7");
    assert!(v["cookie"].is_null(), "le cookie ne doit pas passer : {v}");

    // Le flux audio.
    let (entete, corps) = http(&app.conn, "GET /stream/piste.flac", 4096).await;
    assert!(entete.starts_with("HTTP/1.1 200"), "{entete}");
    assert!(corps.len() >= 4096, "flux trop court : {}", corps.len());

    // Rien d'autre : ni une route hors de l'écoute, ni une échappée par `..`.
    for ligne in [
        "GET /autre",
        "GET /api/v1/../autre",
        "GET /api/v1/%2e%2e/autre",
        "GET /stream/../autre",
        "GET //autre",
    ] {
        let (entete, _) = http(&app.conn, ligne, usize::MAX).await;
        assert!(entete.starts_with("HTTP/1.1 404"), "{ligne} : {entete}");
    }
    let (entete, _) = http(&app.conn, "POST /stream/piste.flac", usize::MAX).await;
    assert!(entete.starts_with("HTTP/1.1 405"), "{entete}");
    assert!(
        !b.journal.lock().unwrap().iter().any(|c| c == "/autre"),
        "une route hors de l'ecoute a ete appelee"
    );
    drop(app);
}

#[tokio::test]
async fn une_deuxieme_ecoute_coupe_la_premiere() {
    let b = banc().await;
    let mut premiere = ouvrir(&b, &signer(&rev("j-1"))).await;
    assert!(matches!(premiere.reponse, MessageServeur::Ouvert { .. }));
    let (entete, _) = http(&premiere.conn, "GET /stream/a.flac", 2048).await;
    assert!(entete.starts_with("HTTP/1.1 200"));

    // Parcourir ne compte pas : la deuxième session lit l'API, la première
    // continue.
    let seconde = ouvrir(&b, &signer(&rev("j-2"))).await;
    assert!(matches!(seconde.reponse, MessageServeur::Ouvert { .. }));
    let _ = http(&seconde.conn, "GET /api/v1/echo", usize::MAX).await;
    assert_eq!(
        b.moteur.sessions().len(),
        2,
        "parcourir ne doit couper personne"
    );

    // La deuxième se met à jouer : la première est coupée.
    let (entete, _) = http(&seconde.conn, "GET /stream/b.flac", 2048).await;
    assert!(entete.starts_with("HTTP/1.1 200"));
    let coupee = tokio::time::timeout(
        PATIENCE,
        lire_trame::<_, MessageServeur>(&mut premiere.recv),
    )
    .await
    .expect("la premiere ecoute continue")
    .expect("trame de coupure");
    assert_eq!(
        coupee,
        MessageServeur::Coupee {
            motif: "ecoute_reprise_ailleurs".into(),
            appareil: Some("Appareil 2".into()),
        }
    );
    let fin = tokio::time::timeout(PATIENCE, premiere.conn.closed())
        .await
        .expect("la connexion coupee doit se fermer");
    match fin {
        iroh::endpoint::ConnectionError::ApplicationClosed(c) => {
            assert_eq!(c.error_code, iroh::endpoint::VarInt::from_u32(CODE_COUPEE));
            assert!(String::from_utf8_lossy(&c.reason).contains("ecoute_reprise_ailleurs"));
        }
        autre => panic!("fermeture inattendue : {autre:?}"),
    }
    let restantes = b.moteur.sessions();
    assert_eq!(restantes.len(), 1);
    assert_eq!(restantes[0].site_id().as_deref(), Some("site-2"));
    let _ = premiere.send.finish();
}

#[tokio::test]
async fn l_ordre_de_coupure_du_site_a_l_ouverture_coupe_la_session_directe() {
    let b = banc().await;
    let mut premiere = ouvrir(&b, &signer(&rev("j-a"))).await;
    assert!(matches!(premiere.reponse, MessageServeur::Ouvert { .. }));
    // Le site dit : la nouvelle (relais ou direct) prend la main sur site-1.
    *b.site.couper_a_l_ouverture.lock().unwrap() = vec!["site-1".into()];
    let _seconde = ouvrir(&b, &signer(&rev("j-b"))).await;
    let m = tokio::time::timeout(
        PATIENCE,
        lire_trame::<_, MessageServeur>(&mut premiere.recv),
    )
    .await
    .expect("delai")
    .expect("trame");
    assert!(
        matches!(m, MessageServeur::Coupee { ref motif, .. } if motif == "ecoute_reprise_ailleurs"),
        "{m:?}"
    );
}

// ---------------------------------------------------------------------------
// Battement, secours, révocation, renouvellement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn le_battement_rend_compte_et_applique_coupures_et_revocations() {
    let b = banc().await;
    let mut app = ouvrir(&b, &signer(&rev("j-bat"))).await;
    let _ = http(&app.conn, "GET /stream/a.flac", 2048).await;
    b.moteur.battre().await;
    let bat = b
        .site
        .battements
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("battement");
    assert_eq!(bat.sessions.len(), 1);
    assert_eq!(bat.sessions[0].session_id, "site-1");
    assert!(bat.sessions[0].audio_actif);
    assert!(bat.sessions[0].octets_depuis_le_dernier >= 2048);

    // Le site révoque l'appareil : la session est coupée.
    b.site.reponse_battement.lock().unwrap().revoques.did = vec!["appareil-1".into()];
    b.moteur.battre().await;
    let m = tokio::time::timeout(PATIENCE, lire_trame::<_, MessageServeur>(&mut app.recv))
        .await
        .expect("delai")
        .expect("trame");
    assert_eq!(
        m,
        MessageServeur::Coupee {
            motif: "revoquee".into(),
            appareil: None
        }
    );
    assert!(b.moteur.sessions().is_empty());
    // Et l'appareil ne rouvre rien.
    let app = ouvrir(&b, &signer(&rev("j-bat-2"))).await;
    assert_eq!(code_de_refus(&app.reponse), "revoque");
}

#[tokio::test]
async fn site_injoignable_un_laissez_passer_valide_ouvre_en_secours_puis_est_rejoue() {
    let b = banc().await;
    b.site.en_panne.store(true, Ordering::Relaxed);
    let app = ouvrir(&b, &signer(&rev("j-secours"))).await;
    assert!(
        matches!(app.reponse, MessageServeur::Ouvert { secours: true, .. }),
        "{:?}",
        app.reponse
    );
    assert_eq!(b.moteur.sessions()[0].site_id(), None);
    b.site.en_panne.store(false, Ordering::Relaxed);
    b.moteur.battre().await;
    let o = b.site.ouvertures.lock().unwrap().clone();
    assert_eq!(o.len(), 1);
    assert!(
        o[0].secours,
        "l'ouverture rejouee doit etre marquee secours"
    );
    assert_eq!(b.moteur.sessions()[0].site_id().as_deref(), Some("site-1"));
}

#[tokio::test]
async fn le_renouvellement_exige_le_meme_appareil_et_p2p() {
    let b = banc().await;
    let mut app = ouvrir(&b, &signer(&rev("j-r1"))).await;
    let MessageServeur::Ouvert { defi_suivant, .. } = app.reponse.clone() else {
        panic!("{:?}", app.reponse);
    };
    // Même appareil : renouvelé, avec un nouveau défi.
    let r2 = rev("j-r2");
    ecrire_trame(
        &mut app.send,
        &MessageApp::Renouveler {
            laissez_passer: signer(&r2),
            signature: appareil().signer(&message_defi_direct(SID, &defi_suivant)),
        },
    )
    .await
    .unwrap();
    let m = lire_trame::<_, MessageServeur>(&mut app.recv)
        .await
        .unwrap();
    let MessageServeur::Renouvele {
        defi_suivant: d2, ..
    } = m
    else {
        panic!("{m:?}");
    };
    assert_ne!(d2, defi_suivant);
    // Le défi déjà servi ne vaut plus : coupé.
    ecrire_trame(
        &mut app.send,
        &MessageApp::Renouveler {
            laissez_passer: signer(&rev("j-r3")),
            signature: appareil().signer(&message_defi_direct(SID, &defi_suivant)),
        },
    )
    .await
    .unwrap();
    let m = lire_trame::<_, MessageServeur>(&mut app.recv)
        .await
        .unwrap();
    assert!(
        matches!(m, MessageServeur::Coupee { ref motif, .. } if motif == "defi"),
        "{m:?}"
    );

    // Un autre appareil ne renouvelle pas la session d'un premier.
    let mut app = ouvrir(&b, &signer(&rev("j-r4"))).await;
    let MessageServeur::Ouvert { defi_suivant, .. } = app.reponse.clone() else {
        panic!();
    };
    let autre = AppareilDeTest::ed25519(&GRAINE_AUTRE_APPAREIL);
    let mut r = rev("j-r5");
    r.cnf = autre.confirmation();
    ecrire_trame(
        &mut app.send,
        &MessageApp::Renouveler {
            laissez_passer: signer(&r),
            signature: autre.signer(&message_defi_direct(SID, &defi_suivant)),
        },
    )
    .await
    .unwrap();
    let m = lire_trame::<_, MessageServeur>(&mut app.recv)
        .await
        .unwrap();
    assert!(
        matches!(m, MessageServeur::Coupee { ref motif, .. } if motif == "autre_appareil"),
        "{m:?}"
    );
}

// ---------------------------------------------------------------------------
// Réglages, identité, signature des requêtes au site
// ---------------------------------------------------------------------------

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

#[test]
fn eteinte_par_defaut_la_variable_l_emporte_sur_le_reglage() {
    let b = base();
    let r = SettingsRepo::with_backend(b.clone());
    assert!(!active_selon(None, &r), "eteinte par defaut");
    r.set(REGLAGE_ACTIVE, "true").unwrap();
    assert!(active_selon(None, &r));
    assert!(!active_selon(Some("0"), &r), "la variable eteint");
    assert!(!active_selon(Some("n'importe"), &r), "illisible = eteinte");
    r.set(REGLAGE_ACTIVE, "false").unwrap();
    assert!(active_selon(Some("1"), &r), "la variable allume");
    assert!(!active_selon(None, &r));
}

#[test]
fn aucun_relais_iroh_public_par_defaut() {
    let b = base();
    let r = SettingsRepo::with_backend(b.clone());
    if std::env::var(ENV_RELAIS_IROH).is_err() {
        assert_eq!(relais_iroh(&r), None);
        r.set(REGLAGE_RELAIS_IROH, "aucun").unwrap();
        assert_eq!(relais_iroh(&r), None);
        r.set(REGLAGE_RELAIS_IROH, "https://relais.exemple.test")
            .unwrap();
        assert_eq!(
            relais_iroh(&r).as_deref(),
            Some("https://relais.exemple.test")
        );
    }
}

#[tokio::test]
async fn l_identite_d_endpoint_est_la_cle_du_serveur() {
    let ep = ouvrir_endpoint(
        &GRAINE_SERVEUR,
        &ConfigReseau {
            boucle_locale: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(en_b64url(ep.id().as_bytes()), cle_publique_serveur());
    ep.close().await;
}

#[test]
fn les_requetes_au_site_sont_signees_par_la_cle_du_serveur() {
    let cle = tune_laissez_passer::CleServeur::depuis_graine(&GRAINE_SERVEUR).unwrap();
    let corps = br#"{"sessions":[]}"#;
    let chemin = format!("/api/v1/remote/servers/{SID}/heartbeat");
    let s = signer_requete(&cle, "POST", &chemin, SID, 1_793_000_000, corps).unwrap();
    assert_eq!(
        tune_laissez_passer::verifier_requete_serveur(
            &cle.cle_publique(),
            "POST",
            &chemin,
            SID,
            s.horodatage,
            &s.nonce,
            corps,
            &s.signature
        ),
        Ok(())
    );
}

#[test]
fn seules_les_routes_du_relais_sont_atteignables() {
    for ok in [
        "/api/v1/library/albums?x=1",
        "/stream/a.flac",
        "/api/v1/library/tracks/4/audio",
    ] {
        assert!(url_locale(8888, ok).is_some(), "{ok}");
    }
    for non in [
        "/",
        "/autre",
        "/api/v2/x",
        "/api/v1/../x",
        "/api/v1/%2e%2e/x",
        "/api/v1/%2E%2E/x",
        "/stream/../api/../x",
        "//hote/api/v1/x",
        "/api\\v1/x",
        "api/v1/x",
        "/ws",
    ] {
        assert!(url_locale(8888, non).is_none(), "{non}");
    }
    assert!(est_audio("/stream/a.flac"));
    assert!(est_audio("/api/v1/library/tracks/4/audio"));
    assert!(!est_audio("/api/v1/library/albums"));
}
