//! Rôle maître / agent entre deux serveurs Tune (#4626), de bout en bout.
//!
//! Deux VRAIS serveurs sont montés dans le même processus, chacun sur sa
//! socket `127.0.0.1` et sa base `:memory:` : un MAÎTRE et un AGENT. Rien
//! n'est simulé entre eux — l'appairage, les ordres et le flux passent par
//! HTTP, par les routes que la production monte.
//!
//! Ce qui est simulé, et seulement cela :
//! - la sortie LOCALE de l'agent : une sortie enregistreuse de type `local`
//!   qui va chercher le flux à l'URL reçue, comme une sortie locale, et garde
//!   chaque octet. Pas de DAC sur un runner de test ;
//! - la découverte mDNS : le multicast n'est pas fiable sur un runner, donc
//!   l'agent est connu du maître par le registre manuel (`tune_peers`), l'autre
//!   moitié de `/system/peers`, que la route des candidats unit à la
//!   découverte ;
//! - la source : un petit serveur HTTP qui sert un WAV PCM fabriqué ici.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use axum::Router;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::traits::{
    OutputCapabilities, OutputStatus, OutputTarget, PlayMedia, TransportState,
};
use tune_server::state::AppState;

// ---------------------------------------------------------------------------
// Banc d'essai
// ---------------------------------------------------------------------------

/// Un serveur Tune complet, servi sur une vraie socket.
struct Serveur {
    state: AppState,
    port: u16,
}

impl Serveur {
    async fn demarrer() -> Self {
        Self::demarrer_avec(Default::default()).await
    }

    async fn demarrer_avec(config: tune_server::config::TuneConfig) -> Self {
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = ecoute.local_addr().unwrap().port();
        let state = AppState::new(":memory:", port, config).unwrap();
        let app: Router = tune_server::routes::router(state.clone());
        tokio::spawn(async move {
            axum::serve(
                ecoute,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap()
        });
        Self { state, port }
    }

    fn url(&self, chemin: &str) -> String {
        format!("http://127.0.0.1:{}{chemin}", self.port)
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap()
}

/// Le PCM de référence : 1 s de stéréo 16 bits à 44,1 kHz, pseudo-aléatoire
/// (aucune régularité qu'une compression ou un arrondi pourrait épargner).
fn pcm_de_reference() -> Vec<u8> {
    pcm_pseudo_aleatoire(1)
}

/// Octets de PCM par seconde : stéréo 16 bits à 44,1 kHz.
const OCTETS_PAR_SECONDE: usize = 44_100 * 4;

/// `secondes` de PCM de référence (voir [`pcm_de_reference`]).
fn pcm_pseudo_aleatoire(secondes: usize) -> Vec<u8> {
    let mut graine: u32 = 0x4626_4626;
    let mut pcm = Vec::with_capacity(OCTETS_PAR_SECONDE * secondes);
    for _ in 0..44_100 * 2 * secondes {
        graine ^= graine << 13;
        graine ^= graine >> 17;
        graine ^= graine << 5;
        pcm.extend_from_slice(&(graine as u16).to_le_bytes());
    }
    pcm
}

fn wav(pcm: &[u8]) -> Vec<u8> {
    let mut w = Vec::with_capacity(pcm.len() + 44);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&44_100u32.to_le_bytes());
    w.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(pcm);
    w
}

/// La source : sert `octets` à `/piste.wav`.
async fn source(octets: Vec<u8>) -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    let octets = Arc::new(octets);
    let app = Router::new().route(
        "/piste.wav",
        axum::routing::get(move || {
            let octets = octets.clone();
            async move {
                (
                    [(axum::http::header::CONTENT_TYPE, "audio/wav")],
                    octets.as_ref().clone(),
                )
            }
        }),
    );
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    format!("http://127.0.0.1:{port}/piste.wav")
}

/// La sortie locale simulée de l'agent : elle TIRE le flux à l'URL reçue,
/// comme une sortie locale de Tune, et garde chaque octet.
#[derive(Clone)]
struct Enregistreuse {
    recu: Arc<Mutex<Vec<u8>>>,
    etat: Arc<Mutex<(TransportState, Option<String>)>>,
    volume: Arc<Mutex<f64>>,
    muet: Arc<Mutex<bool>>,
    lectures: Arc<AtomicU32>,
    /// #6370 — chaque flux reçu, dans son PROPRE tampon, par ordre de
    /// lecture : un téléchargement encore en cours de l'ancien flux ne peut
    /// pas se mêler au nouveau (déplacement pendant la mise en tampon).
    flux: Arc<Mutex<Vec<Vec<u8>>>>,
    /// #6370 — chaque `seek` reçu par la sortie de l'agent.
    positions: Arc<Mutex<Vec<u64>>>,
    /// #6630 — pour chaque flux, au même rang que `flux` : le
    /// `Content-Length` annoncé, puis comment le corps a fini (`None` tant
    /// qu'il coule, `Err` s'il s'est fermé avant sa longueur annoncée).
    longueurs: Arc<Mutex<Vec<Option<u64>>>>,
    fins: Arc<Mutex<Vec<Option<Result<(), String>>>>>,
}

impl Default for Enregistreuse {
    fn default() -> Self {
        Self {
            recu: Arc::default(),
            etat: Arc::new(Mutex::new((TransportState::Stopped, None))),
            volume: Arc::default(),
            muet: Arc::default(),
            lectures: Arc::default(),
            flux: Arc::default(),
            positions: Arc::default(),
            longueurs: Arc::default(),
            fins: Arc::default(),
        }
    }
}

struct SortieLocaleSimulee(Enregistreuse);

const DEVICE_LOCAL: &str = "local:dac-usb-essai";

#[async_trait::async_trait]
impl OutputTarget for SortieLocaleSimulee {
    fn name(&self) -> &str {
        "DAC USB d'essai"
    }
    fn device_id(&self) -> &str {
        DEVICE_LOCAL
    }
    fn output_type(&self) -> &str {
        "local"
    }
    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, true, true, true, true)
    }
    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        let e = self.0.clone();
        e.lectures.fetch_add(1, Ordering::SeqCst);
        *e.etat.lock().await = (TransportState::Playing, Some(media.url.to_string()));
        e.recu.lock().await.clear();
        // Les deux serveurs du banc écoutent sur `127.0.0.1` ; le maître, lui,
        // construit ses URL de flux avec l'adresse LAN de la machine, comme
        // en production. La sortie simulée va donc chercher le flux par la
        // boucle locale, au même port et au même chemin — ce que la vraie
        // sortie locale fait déjà pour une adresse de la machine (#5639).
        // Entre deux machines réelles, l'agent tire l'adresse LAN telle quelle.
        let mut url = reqwest::Url::parse(media.url).map_err(|e| e.to_string())?;
        let _ = url.set_host(Some("127.0.0.1"));
        let url = url.to_string();
        let rang = {
            let mut flux = e.flux.lock().await;
            flux.push(Vec::new());
            e.longueurs.lock().await.push(None);
            e.fins.lock().await.push(None);
            flux.len() - 1
        };
        tokio::spawn(async move {
            // Morceau par morceau, comme une sortie qui lit son flux : un
            // flux encore ouvert montre déjà ce qu'il a servi.
            let fin = match client().get(&url).send().await {
                Ok(mut r) => {
                    e.longueurs.lock().await[rang] = r.content_length();
                    loop {
                        match r.chunk().await {
                            Ok(Some(morceau)) => {
                                e.flux.lock().await[rang].extend_from_slice(&morceau);
                                e.recu.lock().await.extend_from_slice(&morceau);
                            }
                            Ok(None) => break Ok(()),
                            Err(err) => break Err(format!("{err:?}")),
                        }
                    }
                }
                Err(err) => Err(format!("{err:?}")),
            };
            e.fins.lock().await[rang] = Some(fin);
        });
        Ok(())
    }
    async fn pause(&self) -> Result<(), String> {
        self.0.etat.lock().await.0 = TransportState::Paused;
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        self.0.etat.lock().await.0 = TransportState::Playing;
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        *self.0.etat.lock().await = (TransportState::Stopped, None);
        Ok(())
    }
    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        // Comme `LocalOutput::seek` : la position est notée, le son ne bouge
        // pas — un flux séquentiel ne se déplace qu'en étant relancé.
        self.0.positions.lock().await.push(position_ms);
        Ok(())
    }
    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        *self.0.volume.lock().await = volume;
        Ok(())
    }
    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        *self.0.muet.lock().await = muted;
        Ok(())
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        let (state, uri) = self.0.etat.lock().await.clone();
        Ok(OutputStatus {
            state,
            current_uri: uri,
            volume: *self.0.volume.lock().await,
            muted: *self.0.muet.lock().await,
            ..Default::default()
        })
    }
    async fn is_available(&self) -> bool {
        true
    }
}

/// Un agent avec sa sortie locale simulée.
async fn agent() -> (Serveur, Enregistreuse) {
    let serveur = Serveur::demarrer().await;
    let e = Enregistreuse::default();
    serveur
        .state
        .outputs
        .lock()
        .await
        .register(Box::new(SortieLocaleSimulee(e.clone())));
    (serveur, e)
}

async fn code_de(agent: &Serveur) -> String {
    let r: Value = client()
        .post(agent.url("/api/v1/agent-tune/agent/code"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    r["code"].as_str().unwrap().to_string()
}

async fn appairer(maitre: &Serveur, agent: &Serveur, code: &str) -> reqwest::Response {
    client()
        .post(maitre.url("/api/v1/agent-tune/agents"))
        .json(&json!({ "host": "127.0.0.1", "port": agent.port, "code": code }))
        .send()
        .await
        .unwrap()
}

/// Appaire et rend `(agent_id, zone_id, device_id chez le maître)`.
async fn appaires(maitre: &Serveur, agent: &Serveur) -> (String, i64, String) {
    let code = code_de(agent).await;
    let r = appairer(maitre, agent, &code).await;
    assert_eq!(
        r.status(),
        201,
        "appairage refusé : {}",
        r.text().await.unwrap()
    );
    let corps: Value = r.json().await.unwrap();
    let zone = &corps["zones"][0];
    (
        corps["agent"]["agent_id"].as_str().unwrap().to_string(),
        zone["zone_id"].as_i64().unwrap(),
        zone["device_id"].as_str().unwrap().to_string(),
    )
}

/// La sortie que le maître tient pour cette zone — celle que son
/// orchestrateur appellera.
async fn sortie_du_maitre(maitre: &Serveur, device_id: &str) -> Arc<Mutex<Box<dyn OutputTarget>>> {
    maitre
        .state
        .outputs
        .lock()
        .await
        .get(device_id)
        .unwrap_or_else(|| panic!("aucune sortie {device_id} au registre du maître"))
}

async fn attendre<F, Fut>(mut condition: F, quoi: &str)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if condition().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("délai dépassé : {quoi}");
}

// ---------------------------------------------------------------------------
// De bout en bout
// ---------------------------------------------------------------------------

/// Découverte, appairage, zone créée chez le maître, flux reçu à l'identique,
/// pause, reprise, volume, arrêt — dans l'ordre où un utilisateur les vit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_maitre_joue_sur_la_sortie_d_un_agent_de_bout_en_bout() {
    let (agent, sortie_agent) = agent().await;
    let maitre = Serveur::demarrer().await;

    // 1. Découverte : l'agent est un pair connu du maître, et il s'annonce.
    SettingsRepo::with_backend(maitre.state.backend.clone())
        .set(
            "tune_peers",
            &json!([{ "host": "127.0.0.1", "port": agent.port }]).to_string(),
        )
        .unwrap();
    let identite_agent: Value = client()
        .get(agent.url("/api/v1/agent-tune/agent"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let agent_id = identite_agent["agent_id"].as_str().unwrap().to_string();
    assert_eq!(
        identite_agent["sorties"][0]["device_id"], DEVICE_LOCAL,
        "l'agent doit prêter sa sortie locale : {identite_agent}"
    );
    let liste: Value = client()
        .get(maitre.url("/api/v1/agent-tune/agents"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let candidat = liste["candidats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["agent_id"] == agent_id.as_str())
        .unwrap_or_else(|| panic!("l'agent doit figurer parmi les candidats : {liste}"));
    assert_eq!(candidat["appaire"], false);

    // 2. Appairage par le code affiché sur l'agent.
    let (agent_id_appaire, zone_id, device_id) = appaires(&maitre, &agent).await;
    assert_eq!(agent_id_appaire, agent_id);
    assert_eq!(device_id, format!("tune-agent:{agent_id}:{DEVICE_LOCAL}"));

    // 3. La zone existe chez le maître, rattachée à cette sortie.
    let zone = ZoneRepo::with_backend(maitre.state.backend.clone())
        .get_by_device_id(&device_id)
        .unwrap()
        .expect("la zone de la sortie prêtée doit exister chez le maître");
    assert_eq!(zone.id, Some(zone_id));
    assert_eq!(zone.output_type.as_deref(), Some("tune_agent"));
    assert!(
        zone.name.contains("DAC USB d'essai"),
        "la zone doit porter le nom de la sortie : {}",
        zone.name
    );
    let sortie = sortie_du_maitre(&maitre, &device_id).await;
    assert_eq!(sortie.lock().await.output_type(), "tune_agent");

    // 4. Le flux : l'agent reçoit À L'IDENTIQUE ce que la source sert.
    let reference = wav(&pcm_de_reference());
    let url = source(reference.clone()).await;
    sortie
        .lock()
        .await
        .play_url(&url, "audio/wav", Some("Piste"), None)
        .await
        .expect("le maître doit pouvoir lancer la lecture sur l'agent");
    attendre(
        || {
            let e = sortie_agent.clone();
            let n = reference.len();
            async move { e.recu.lock().await.len() >= n }
        },
        "réception du flux par l'agent",
    )
    .await;
    assert!(
        *sortie_agent.recu.lock().await == reference,
        "le PCM reçu par la sortie de l'agent doit être identique, octet pour octet"
    );
    let statut = sortie.lock().await.get_status().await.unwrap();
    assert_eq!(statut.state, TransportState::Playing);

    // 5. Pause, puis reprise.
    sortie.lock().await.pause().await.unwrap();
    assert_eq!(sortie_agent.etat.lock().await.0, TransportState::Paused);
    assert_eq!(
        sortie.lock().await.get_status().await.unwrap().state,
        TransportState::Paused,
        "le maître doit LIRE la pause chez l'agent"
    );
    sortie.lock().await.resume().await.unwrap();
    assert_eq!(sortie_agent.etat.lock().await.0, TransportState::Playing);

    // 6. Volume et sourdine : portés par la sortie de l'agent.
    sortie.lock().await.set_volume(0.42).await.unwrap();
    assert_eq!(*sortie_agent.volume.lock().await, 0.42);
    assert_eq!(sortie.lock().await.get_status().await.unwrap().volume, 0.42);
    sortie.lock().await.set_mute(true).await.unwrap();
    assert!(*sortie_agent.muet.lock().await);

    // 7. Arrêt : la sortie est rendue.
    sortie.lock().await.stop().await.unwrap();
    assert_eq!(sortie_agent.etat.lock().await.0, TransportState::Stopped);
}

/// Le même flux, mais lancé par la ROUTE de lecture du maître : c'est son
/// orchestrateur, et non le test, qui choisit l'URL et appelle la sortie.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_route_de_lecture_du_maitre_atteint_la_sortie_de_l_agent() {
    let (agent, sortie_agent) = agent().await;
    let maitre = Serveur::demarrer().await;
    let (_, zone_id, _) = appaires(&maitre, &agent).await;

    let pcm = pcm_de_reference();
    let url = source(wav(&pcm)).await;
    let r = client()
        .post(maitre.url(&format!("/api/v1/zones/{zone_id}/play")))
        .json(&json!({ "source": "upnp", "source_id": url, "title": "Piste" }))
        .send()
        .await
        .unwrap();
    let code = r.status();
    assert!(
        code.is_success(),
        "la lecture doit partir : {code} {}",
        r.text().await.unwrap()
    );
    attendre(
        || {
            let e = sortie_agent.clone();
            async move { e.lectures.load(Ordering::SeqCst) > 0 }
        },
        "ordre de lecture reçu par l'agent",
    )
    .await;
    attendre(
        || {
            let e = sortie_agent.clone();
            let n = pcm.len();
            async move { e.recu.lock().await.len() >= n }
        },
        "réception du flux par l'agent",
    )
    .await;
    // Le conteneur peut être réécrit par le maître ; les ÉCHANTILLONS, non.
    let recu = sortie_agent.recu.lock().await.clone();
    eprintln!(
        "URL donnée à l'agent par l'orchestrateur du maître : {:?}",
        sortie_agent.etat.lock().await.1
    );
    assert!(
        echantillons(&recu).starts_with(&pcm),
        "les échantillons reçus par l'agent doivent être ceux de la source \
         ({} octets reçus, URL : {:?})",
        recu.len(),
        sortie_agent.etat.lock().await.1
    );

    // Pause et volume par les routes du maître.
    let r = client()
        .post(maitre.url(&format!("/api/v1/zones/{zone_id}/pause")))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "pause : {}", r.status());
    attendre(
        || {
            let e = sortie_agent.clone();
            async move { e.etat.lock().await.0 == TransportState::Paused }
        },
        "pause appliquée chez l'agent",
    )
    .await;
    let r = client()
        .post(maitre.url(&format!("/api/v1/zones/{zone_id}/volume")))
        .json(&json!({ "volume": 0.3 }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "volume : {}", r.status());
    attendre(
        || {
            let e = sortie_agent.clone();
            async move { (*e.volume.lock().await - 0.3).abs() < 1e-6 }
        },
        "volume appliqué chez l'agent",
    )
    .await;
}

/// Une piste de la BIBLIOTHÈQUE du maître (un WAV sur son disque) : l'URL
/// vient alors du serveur de flux du maître, et c'est elle que l'agent tire.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_piste_de_la_bibliotheque_du_maitre_arrive_intacte_chez_l_agent() {
    let (agent, sortie_agent) = agent().await;
    let maitre = Serveur::demarrer().await;
    let (_, zone_id, _) = appaires(&maitre, &agent).await;

    let dossier = tempfile::tempdir().unwrap();
    let pcm = pcm_de_reference();
    let track_id = piste_de_bibliotheque(&maitre, &dossier, &pcm);
    jouer_la_piste(&maitre, zone_id, track_id).await;
    for _ in 0..400 {
        if sortie_agent.recu.lock().await.len() >= pcm.len() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let url = sortie_agent.etat.lock().await.1.clone().unwrap_or_default();
    eprintln!(
        "chemin de bibliothèque donné à l'agent : {}",
        reqwest::Url::parse(&url)
            .map(|u| u.path().to_string())
            .unwrap_or_default()
    );
    assert!(
        url.contains(&format!(":{}/", maitre.port)),
        "l'agent doit tirer le flux du MAÎTRE : {url}"
    );
    let recu = sortie_agent.recu.lock().await.clone();
    assert!(
        echantillons(&recu).starts_with(&pcm),
        "échantillons altérés entre la bibliothèque du maître et l'agent \
         ({} octets reçus depuis {url})",
        recu.len()
    );
}

/// Inscrit dans la bibliothèque de `serveur` un WAV portant `pcm`, écrit dans
/// `dossier`, et rend l'identifiant de la piste.
fn piste_de_bibliotheque(serveur: &Serveur, dossier: &tempfile::TempDir, pcm: &[u8]) -> i64 {
    let chemin = dossier.path().join("piste.wav");
    std::fs::write(&chemin, wav(pcm)).unwrap();
    let pistes = tune_core::db::track_repo::TrackRepo::with_backend(serveur.state.backend.clone());
    let mut t = tune_core::db::models::Track::new("Piste de bibliothèque".into());
    t.file_path = Some(chemin.to_string_lossy().into_owned());
    t.format = Some("wav".into());
    t.sample_rate = Some(44_100);
    t.bit_depth = Some(16);
    t.channels = 2;
    t.duration_ms = (pcm.len() * 1000 / OCTETS_PAR_SECONDE) as i64;
    t.file_size = Some(std::fs::metadata(&chemin).unwrap().len() as i64);
    pistes.create(&t).unwrap()
}

/// Lance `track_id` sur la zone par la route de lecture de `serveur`.
async fn jouer_la_piste(serveur: &Serveur, zone_id: i64, track_id: i64) {
    let r = client()
        .post(serveur.url(&format!("/api/v1/zones/{zone_id}/play")))
        .json(&json!({ "track_id": track_id }))
        .send()
        .await
        .unwrap();
    let code = r.status();
    assert!(
        code.is_success(),
        "lecture : {code} {}",
        r.text().await.unwrap()
    );
}

/// Les échantillons d'un flux WAV : ce qui suit le bloc `data`.
fn echantillons(flux: &[u8]) -> &[u8] {
    let debut = flux
        .windows(4)
        .position(|w| w == b"data")
        .map(|p| p + 8)
        .unwrap_or(0);
    &flux[debut..]
}

// ---------------------------------------------------------------------------
// #6370 — se déplacer dans la piste sur une zone d'agent
// ---------------------------------------------------------------------------

/// Durée de la piste des essais de déplacement.
const SECONDES_6370: usize = 4;
/// Ce qu'un flux neuf doit avoir livré pour être jugé : un quart de seconde de
/// PCM pseudo-aléatoire, qu'aucun autre endroit de la piste ne reproduit.
const ECHANTILLONS_MINIMUM_6370: usize = OCTETS_PAR_SECONDE / 4;

/// Déplace la lecture de la zone par la route du serveur.
async fn deplacer(serveur: &Serveur, zone_id: i64, position_ms: u64) {
    let r = client()
        .post(serveur.url(&format!("/api/v1/zones/{zone_id}/seek")))
        .json(&json!({ "position_ms": position_ms }))
        .send()
        .await
        .unwrap();
    let code = r.status();
    assert!(
        code.is_success(),
        "déplacement à {position_ms} ms : {code} {}",
        r.text().await.unwrap()
    );
}

/// Attend que la sortie ait reçu au moins `attendu` octets d'échantillons
/// (en 16 bits) de son `n`-ième flux, et le rend.
async fn flux_recu(sortie: &Enregistreuse, n: usize, attendu: usize) -> Vec<u8> {
    for _ in 0..1_200 {
        {
            let flux = sortie.flux.lock().await;
            if flux.len() >= n && echantillons_16_bits(&flux[n - 1]).len() >= attendu {
                return flux[n - 1].clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let flux = sortie.flux.lock().await;
    panic!(
        "flux n° {n} jamais reçu ({attendu} octets attendus) : {} flux reçu(s), longueurs {:?}, \
         dernière URL {:?}",
        flux.len(),
        flux.iter().map(|f| f.len()).collect::<Vec<_>>(),
        sortie.etat.lock().await.1
    );
}

/// `(format, bits par échantillon)` de l'en-tête WAV d'un flux.
fn format_wav(flux: &[u8]) -> Option<(u16, u16)> {
    let fmt = flux.windows(4).position(|w| w == b"fmt ")? + 8;
    let champ = |o: usize| {
        flux.get(fmt + o..fmt + o + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    Some((champ(0)?, champ(14)?))
}

/// Les échantillons d'un flux WAV ramenés en 16 bits : un flux décodé pour
/// une sortie locale peut être servi sur 24 ou 32 bits entiers, dont les deux
/// octets de poids fort sont alors ceux de la source 16 bits.
fn echantillons_16_bits(flux: &[u8]) -> Vec<u8> {
    let octets = match format_wav(flux) {
        Some((1, bits)) if bits >= 16 => bits as usize / 8,
        _ => 2,
    };
    echantillons(flux)
        .chunks_exact(octets)
        .flat_map(|e| [e[octets - 2], e[octets - 1]])
        .collect()
}

/// Le flux neuf commence-t-il à `position_ms` de la piste ? Rend un constat
/// lisible plutôt qu'un booléen muet.
///
/// Seul le DÉBUT du flux est comparé, sur ce qui en est arrivé : #6370
/// demande que le flux commence au bon endroit. Qu'il aille jusqu'au bout de
/// sa longueur annoncée, c'est l'objet de #6630 ([`flux_complet`]).
fn commence_a(flux: &[u8], pcm: &[u8], position_ms: u64) -> Result<(), String> {
    let octet = position_ms as usize * OCTETS_PAR_SECONDE / 1000;
    let recu = echantillons_16_bits(flux);
    let reste = &pcm[octet..];
    if !recu.is_empty() && recu.len() <= reste.len() && reste.starts_with(&recu) {
        return Ok(());
    }
    let depart = (0..pcm.len())
        .step_by(4)
        .find(|&o| recu.len() >= 64 && pcm[o..].starts_with(&recu[..64]));
    Err(format!(
        "le flux neuf devait commencer à l'octet {octet} ({position_ms} ms) de la \
         piste ; il commence à l'octet {depart:?} ({} octets d'échantillons reçus, \
         format WAV {:?})",
        recu.len(),
        format_wav(flux)
    ))
}

/// Une zone d'agent, une piste en lecture, un déplacement : la sortie de
/// l'agent reçoit un NOUVEAU flux qui commence à la position demandée, et le
/// maître rapporte la position de la piste, pas celle du flux neuf.
///
/// Avant le correctif, le maître envoyait `Commande::Position` : la sortie
/// locale de l'agent notait la position, ne relançait rien, et le son restait
/// où il était (fil 1867).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_deplacement_sur_une_zone_d_agent_relance_le_flux_a_la_position_6370() {
    let (agent, sortie_agent) = agent().await;
    let maitre = Serveur::demarrer().await;
    let (_, zone_id, device_id) = appaires(&maitre, &agent).await;
    let dossier = tempfile::tempdir().unwrap();
    let pcm = pcm_pseudo_aleatoire(SECONDES_6370);
    let track_id = piste_de_bibliotheque(&maitre, &dossier, &pcm);

    jouer_la_piste(&maitre, zone_id, track_id).await;
    let premier = flux_recu(&sortie_agent, 1, pcm.len()).await;
    commence_a(&premier, &pcm, 0).unwrap();

    deplacer(&maitre, zone_id, 2_000).await;
    let neuf = flux_recu(&sortie_agent, 2, ECHANTILLONS_MINIMUM_6370).await;
    commence_a(&neuf, &pcm, 2_000).unwrap();
    assert_eq!(
        sortie_agent.etat.lock().await.0,
        TransportState::Playing,
        "le flux neuf doit être en lecture chez l'agent"
    );
    let statut = sortie_du_maitre(&maitre, &device_id)
        .await
        .lock()
        .await
        .get_status()
        .await
        .unwrap();
    assert!(
        statut.position_ms >= 2_000,
        "le maître doit rapporter la position dans la PISTE (≥ 2000 ms), pas \
         celle du flux neuf : {} ms",
        statut.position_ms
    );
}

/// En pause, puis déplacement : le flux est relancé à la position, comme sur
/// une sortie locale du maître.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn en_pause_un_deplacement_sur_une_zone_d_agent_relance_le_flux_a_la_position_6370() {
    let (agent, sortie_agent) = agent().await;
    let maitre = Serveur::demarrer().await;
    let (_, zone_id, _) = appaires(&maitre, &agent).await;
    let dossier = tempfile::tempdir().unwrap();
    let pcm = pcm_pseudo_aleatoire(SECONDES_6370);
    let track_id = piste_de_bibliotheque(&maitre, &dossier, &pcm);

    jouer_la_piste(&maitre, zone_id, track_id).await;
    flux_recu(&sortie_agent, 1, pcm.len()).await;
    let r = client()
        .post(maitre.url(&format!("/api/v1/zones/{zone_id}/pause")))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "pause : {}", r.status());
    attendre(
        || {
            let e = sortie_agent.clone();
            async move { e.etat.lock().await.0 == TransportState::Paused }
        },
        "pause appliquée chez l'agent",
    )
    .await;

    deplacer(&maitre, zone_id, 1_500).await;
    let neuf = flux_recu(&sortie_agent, 2, ECHANTILLONS_MINIMUM_6370).await;
    commence_a(&neuf, &pcm, 1_500).unwrap();
}

/// Déplacement pendant la mise en tampon : le premier flux n'est pas encore
/// arrivé quand l'auditeur se déplace. Le DERNIER flux reçu commence à la
/// position.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_deplacement_pendant_la_mise_en_tampon_relance_le_flux_a_la_position_6370() {
    let (agent, sortie_agent) = agent().await;
    let maitre = Serveur::demarrer().await;
    let (_, zone_id, _) = appaires(&maitre, &agent).await;
    let dossier = tempfile::tempdir().unwrap();
    let pcm = pcm_pseudo_aleatoire(SECONDES_6370);
    let track_id = piste_de_bibliotheque(&maitre, &dossier, &pcm);

    jouer_la_piste(&maitre, zone_id, track_id).await;
    // Seulement l'ORDRE de lecture : le flux, lui, n'est pas attendu.
    attendre(
        || {
            let e = sortie_agent.clone();
            async move { e.lectures.load(Ordering::SeqCst) > 0 }
        },
        "ordre de lecture reçu par l'agent",
    )
    .await;
    deplacer(&maitre, zone_id, 1_000).await;
    let neuf = flux_recu(&sortie_agent, 2, ECHANTILLONS_MINIMUM_6370).await;
    commence_a(&neuf, &pcm, 1_000).unwrap();
    assert_eq!(
        sortie_agent.flux.lock().await.len(),
        2,
        "un seul flux neuf par déplacement"
    );
}

/// Témoin : sur une sortie LOCALE (ici l'agent joué pour lui-même, sans
/// maître), le déplacement relance déjà le flux à la position. Le correctif
/// de #6370 ne doit rien y changer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn temoin_6370_un_deplacement_sur_une_sortie_locale_relance_toujours_le_flux() {
    let (serveur, sortie_locale) = agent().await;
    let (zone_id, _) = ZoneRepo::with_backend(serveur.state.backend.clone())
        .get_or_create("DAC USB d'essai", Some("local"), DEVICE_LOCAL)
        .unwrap();
    let dossier = tempfile::tempdir().unwrap();
    let pcm = pcm_pseudo_aleatoire(SECONDES_6370);
    let track_id = piste_de_bibliotheque(&serveur, &dossier, &pcm);

    jouer_la_piste(&serveur, zone_id, track_id).await;
    flux_recu(&sortie_locale, 1, pcm.len()).await;
    deplacer(&serveur, zone_id, 2_000).await;
    let neuf = flux_recu(&sortie_locale, 2, ECHANTILLONS_MINIMUM_6370).await;
    commence_a(&neuf, &pcm, 2_000).unwrap();
}

// ---------------------------------------------------------------------------
// #6630 — un flux relancé à une position annonce la longueur qu'il sert
// ---------------------------------------------------------------------------

/// Attend la FIN du `n`-ième flux reçu par la sortie et le rend avec son
/// `Content-Length`. Échoue si le corps s'est fermé avant sa longueur
/// annoncée, ou si ce qui est arrivé n'est pas ce qui était annoncé.
async fn flux_complet(sortie: &Enregistreuse, n: usize) -> (Vec<u8>, Option<u64>) {
    for _ in 0..1_200 {
        let fin = sortie.fins.lock().await.get(n - 1).cloned().flatten();
        if let Some(fin) = fin {
            let flux = sortie.flux.lock().await[n - 1].clone();
            let annonce = sortie.longueurs.lock().await[n - 1];
            if let Err(err) = fin {
                panic!(
                    "#6630 — le flux n° {n} s'est fermé avant sa longueur annoncée : \
                     Content-Length {annonce:?}, {} octets reçus ({err})",
                    flux.len()
                );
            }
            if let Some(annonce) = annonce {
                assert_eq!(
                    flux.len() as u64,
                    annonce,
                    "#6630 — le flux n° {n} ne livre pas son Content-Length"
                );
            }
            return (flux, annonce);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("flux n° {n} jamais terminé");
}

/// Tout le flux, échantillon pour échantillon, est-il la piste à partir de
/// `position_ms` jusqu'à sa FIN ?
fn est_la_piste_depuis(flux: &[u8], pcm: &[u8], position_ms: u64) -> Result<(), String> {
    let octet = position_ms as usize * OCTETS_PAR_SECONDE / 1000;
    let recu = echantillons_16_bits(flux);
    if recu == pcm[octet..] {
        return Ok(());
    }
    Err(format!(
        "le flux devait porter la piste de l'octet {octet} ({position_ms} ms) à la fin, \
         soit {} octets d'échantillons ; il en porte {} (format WAV {:?})",
        pcm.len() - octet,
        recu.len(),
        format_wav(flux)
    ))
}

/// Sortie LOCALE (flux décodé, toujours) : la lecture depuis le début est
/// servie en entier, sous un Content-Length tenu — comme avant ; relancée à
/// 2 s, elle annonce la longueur qu'elle sert, et la sert jusqu'au bout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sortie_locale_un_flux_relance_a_une_position_annonce_la_longueur_servie_6630() {
    let (serveur, sortie_locale) = agent().await;
    let (zone_id, _) = ZoneRepo::with_backend(serveur.state.backend.clone())
        .get_or_create("DAC USB d'essai", Some("local"), DEVICE_LOCAL)
        .unwrap();
    let dossier = tempfile::tempdir().unwrap();
    let pcm = pcm_pseudo_aleatoire(SECONDES_6370);
    let track_id = piste_de_bibliotheque(&serveur, &dossier, &pcm);

    jouer_la_piste(&serveur, zone_id, track_id).await;
    let (premier, annonce) = flux_complet(&sortie_locale, 1).await;
    assert!(
        annonce.is_some(),
        "depuis le début : Content-Length attendu"
    );
    est_la_piste_depuis(&premier, &pcm, 0).unwrap();

    deplacer(&serveur, zone_id, 2_000).await;
    let (neuf, annonce) = flux_complet(&sortie_locale, 2).await;
    assert!(
        annonce.is_some(),
        "relancé à une position : Content-Length attendu"
    );
    est_la_piste_depuis(&neuf, &pcm, 2_000).unwrap();
}

/// Zone d'AGENT : depuis le début, le fichier est servi tel quel (voie
/// fichier direct) ; relancé à 1,5 s, le flux est décodé à partir de la
/// position. Les deux livrent exactement leur Content-Length.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zone_d_agent_fichier_direct_et_flux_decode_tiennent_leur_longueur_6630() {
    let (agent, sortie_agent) = agent().await;
    let maitre = Serveur::demarrer().await;
    let (_, zone_id, _) = appaires(&maitre, &agent).await;
    let dossier = tempfile::tempdir().unwrap();
    let pcm = pcm_pseudo_aleatoire(SECONDES_6370);
    let track_id = piste_de_bibliotheque(&maitre, &dossier, &pcm);

    jouer_la_piste(&maitre, zone_id, track_id).await;
    let (premier, annonce) = flux_complet(&sortie_agent, 1).await;
    assert_eq!(
        annonce,
        Some(wav(&pcm).len() as u64),
        "depuis le début, le fichier part tel quel"
    );
    assert_eq!(premier, wav(&pcm), "fichier direct altéré");

    deplacer(&maitre, zone_id, 1_500).await;
    let (neuf, annonce) = flux_complet(&sortie_agent, 2).await;
    assert!(
        annonce.is_some(),
        "relancé à une position : Content-Length attendu"
    );
    est_la_piste_depuis(&neuf, &pcm, 1_500).unwrap();
}

// ---------------------------------------------------------------------------
// Appairage : aucun accès libre
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sans_jeton_valide_l_agent_refuse_tout_ordre() {
    let (agent, sortie_agent) = agent().await;
    // Un maître EST appairé : le refus ne vient pas d'une liste vide.
    let maitre = Serveur::demarrer().await;
    appaires(&maitre, &agent).await;
    let ordre =
        json!({ "device_id": DEVICE_LOCAL, "commande": { "commande": "volume", "volume": 0.9 } });
    for jeton in [None, Some("jeton-invente")] {
        let mut r = client()
            .post(agent.url("/agent-tune/sorties/commande"))
            .json(&ordre);
        if let Some(j) = jeton {
            r = r.header("x-tune-agent-jeton", j);
        }
        assert_eq!(r.send().await.unwrap().status(), 401, "jeton {jeton:?}");
        let r = client().get(agent.url("/agent-tune/sorties"));
        let r = match jeton {
            Some(j) => r.header("x-tune-agent-jeton", j),
            None => r,
        };
        assert_eq!(r.send().await.unwrap().status(), 401);
    }
    assert_eq!(
        *sortie_agent.volume.lock().await,
        0.0,
        "aucun ordre ne doit passer"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn le_code_est_requis_a_usage_unique_et_brule_apres_cinq_essais() {
    let (agent, _) = agent().await;
    let maitre = Serveur::demarrer().await;

    // Aucun code émis : refus.
    assert_eq!(appairer(&maitre, &agent, "123456").await.status(), 502);

    // Cinq mauvais essais brûlent le code, même le bon ne passe plus.
    let code = code_de(&agent).await;
    let faux = if code == "000000" { "000001" } else { "000000" };
    for _ in 0..5 {
        assert_eq!(appairer(&maitre, &agent, faux).await.status(), 502);
    }
    assert_eq!(appairer(&maitre, &agent, &code).await.status(), 502);
    assert!(
        maitre_sans_agent(&maitre).await,
        "aucun agent ne doit avoir été appairé"
    );

    // Un code neuf passe une fois, et une seule.
    let code = code_de(&agent).await;
    assert_eq!(appairer(&maitre, &agent, &code).await.status(), 201);
    assert_eq!(appairer(&maitre, &agent, &code).await.status(), 502);
}

async fn maitre_sans_agent(maitre: &Serveur) -> bool {
    tune_server::agent_tune::maitre::agents(&maitre.state).is_empty()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l_agent_ne_garde_que_l_empreinte_du_jeton() {
    let (agent, _) = agent().await;
    let maitre = Serveur::demarrer().await;
    appaires(&maitre, &agent).await;
    let jeton = tune_server::agent_tune::maitre::agents(&maitre.state)[0]
        .jeton
        .clone();
    let cote_agent = SettingsRepo::with_backend(agent.state.backend.clone())
        .get("agent_tune_maitres")
        .unwrap()
        .unwrap();
    assert!(
        !cote_agent.contains(&jeton),
        "le jeton ne doit pas être stocké en clair chez l'agent"
    );
    assert!(cote_agent.contains(&tune_server::agent_tune::empreinte(&jeton)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn les_secrets_d_appairage_ne_sortent_pas_dans_une_sauvegarde() {
    let (agent, _) = agent().await;
    let maitre = Serveur::demarrer().await;
    appaires(&maitre, &agent).await;
    for state in [&maitre.state, &agent.state] {
        let instantane = tune_core::config_backup::export_config(&state.backend).unwrap();
        let cles: Vec<&str> = instantane
            .settings
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        for cle in [
            "agent_tune_identite",
            "agent_tune_agents",
            "agent_tune_maitres",
        ] {
            assert!(!cles.contains(&cle), "{cle} ne doit pas être exporté");
        }
    }
}

// ---------------------------------------------------------------------------
// L'agent utilisé seul
// ---------------------------------------------------------------------------

/// Une lecture locale en cours sur l'agent n'est pas coupée par le maître.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_lecture_locale_en_cours_n_est_pas_coupee_par_le_maitre() {
    let (agent, sortie_agent) = agent().await;
    let maitre = Serveur::demarrer().await;
    let (_, _, device_id) = appaires(&maitre, &agent).await;

    // La zone locale de l'agent joue.
    let locale = source(wav(&[0u8; 400])).await;
    let sortie_locale = agent.state.outputs.lock().await.get(DEVICE_LOCAL).unwrap();
    sortie_locale
        .lock()
        .await
        .play_url(&locale, "audio/wav", None, None)
        .await
        .unwrap();

    let sortie = sortie_du_maitre(&maitre, &device_id).await;
    let url = source(wav(&pcm_de_reference())).await;
    let refus = sortie
        .lock()
        .await
        .play_url(&url, "audio/wav", None, None)
        .await
        .expect_err("le maître ne doit pas couper une lecture locale");
    assert!(refus.contains("409"), "refus attendu en 409 : {refus}");
    assert_eq!(
        sortie_agent.etat.lock().await.1.as_deref(),
        Some(locale.as_str()),
        "la lecture locale doit continuer"
    );
    assert!(sortie.lock().await.set_volume(1.0).await.is_err());
    assert_eq!(
        sortie.lock().await.get_status().await.unwrap().state,
        TransportState::Stopped,
        "le maître ne doit pas lire la lecture locale comme la sienne"
    );
}

/// Reprise sur place : la zone locale de l'agent reprend la sortie, le
/// maître la voit arrêtée et ne peut plus la piloter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_reprise_sur_place_rompt_le_bail_du_maitre() {
    let (agent, _) = agent().await;
    let maitre = Serveur::demarrer().await;
    let (_, _, device_id) = appaires(&maitre, &agent).await;
    let sortie = sortie_du_maitre(&maitre, &device_id).await;
    let url = source(wav(&pcm_de_reference())).await;
    sortie
        .lock()
        .await
        .play_url(&url, "audio/wav", None, None)
        .await
        .unwrap();

    let locale = source(wav(&[1u8; 400]))
        .await
        .replace("piste.wav", "piste.wav?locale=1");
    agent
        .state
        .outputs
        .lock()
        .await
        .get(DEVICE_LOCAL)
        .unwrap()
        .lock()
        .await
        .play_url(&locale, "audio/wav", None, None)
        .await
        .unwrap();

    assert_eq!(
        sortie.lock().await.get_status().await.unwrap().state,
        TransportState::Stopped
    );
    assert!(sortie.lock().await.pause().await.is_err());
}

// ---------------------------------------------------------------------------
// Oubli, des deux côtés
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l_agent_qui_revoque_le_maitre_coupe_ses_ordres() {
    let (agent, _) = agent().await;
    let maitre = Serveur::demarrer().await;
    let (_, _, device_id) = appaires(&maitre, &agent).await;
    // Prise AVANT la révocation : le maître, prévenu, retire ensuite la
    // sortie de son registre.
    let sortie = sortie_du_maitre(&maitre, &device_id).await;
    let (maitre_id, _) = tune_server::agent_tune::identite(&maitre.state);
    let r = client()
        .delete(agent.url(&format!("/api/v1/agent-tune/agent/maitres/{maitre_id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let refus = sortie.lock().await.set_volume(0.5).await.unwrap_err();
    assert!(refus.contains("401"), "{refus}");
}

/// Une zone visible (en ligne ou non) chez le maître pour ce `device_id` ?
fn zone_visible(maitre: &Serveur, device_id: &str) -> Option<tune_core::db::zone_repo::Zone> {
    ZoneRepo::with_backend(maitre.state.backend.clone())
        .list()
        .unwrap()
        .into_iter()
        .find(|z| z.output_device_id.as_deref() == Some(device_id))
}

/// Révocation côté MAÎTRE : l'utilisateur du maître oublie l'agent. Ses
/// sorties quittent le registre, sa zone est SUPPRIMÉE (décision du 08/10),
/// et l'agent oublie ce maître.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn le_maitre_qui_oublie_l_agent_retire_ses_sorties_et_supprime_ses_zones() {
    let (agent, _) = agent().await;
    let maitre = Serveur::demarrer().await;
    let (agent_id, zone_id, device_id) = appaires(&maitre, &agent).await;
    assert!(zone_visible(&maitre, &device_id).is_some());
    let r = client()
        .delete(maitre.url(&format!("/api/v1/agent-tune/agents/{agent_id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(!maitre.state.outputs.lock().await.contains(&device_id));
    assert!(
        zone_visible(&maitre, &device_id).is_none(),
        "la zone d'un agent oublié doit être supprimée chez le maître"
    );
    let liste: Value = client()
        .get(maitre.url("/api/v1/zones"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !liste.to_string().contains(&device_id),
        "GET /zones ne doit plus montrer la zone {zone_id} : {liste}"
    );
    // Et l'agent a oublié ce maître.
    attendre(
        || {
            let s = agent.state.clone();
            async move { tune_server::agent_tune::agent::maitres(&s).is_empty() }
        },
        "l'agent oublie le maître",
    )
    .await;
}

/// Révocation côté AGENT : l'utilisateur de l'agent retire le maître. Le
/// maître, prévenu, vérifie la révocation puis supprime la zone. Un nouvel
/// appairage la fait revenir : il vaut consentement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l_agent_qui_revoque_le_maitre_fait_supprimer_ses_zones_chez_le_maitre() {
    let (agent, _) = agent().await;
    let maitre = Serveur::demarrer().await;
    let (agent_id, zone_id, device_id) = appaires(&maitre, &agent).await;
    let (maitre_id, _) = tune_server::agent_tune::identite(&maitre.state);
    let r = client()
        .delete(agent.url(&format!("/api/v1/agent-tune/agent/maitres/{maitre_id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    attendre(
        || {
            let maitre = &maitre;
            let device_id = &device_id;
            let agent_id = &agent_id;
            async move {
                // La zone est masquée AVANT le retrait asynchrone de la sortie.
                // Attendre la révocation entière, pas cet état intermédiaire.
                zone_visible(maitre, device_id).is_none()
                    && !maitre.state.outputs.lock().await.contains(device_id)
                    && tune_server::agent_tune::maitre::agents(&maitre.state)
                        .iter()
                        .all(|a| &a.agent_id != agent_id)
            }
        },
        "révocation incomplète : zone, sortie ou appairage encore présent chez le maître",
    )
    .await;
    assert!(!maitre.state.outputs.lock().await.contains(&device_id));
    assert!(
        tune_server::agent_tune::maitre::agents(&maitre.state)
            .iter()
            .all(|a| a.agent_id != agent_id),
        "le maître doit oublier un agent qui l'a révoqué"
    );

    // Nouvel appairage : la MÊME zone revient, visible et en ligne.
    let (_, zone_id_2, device_id_2) = appaires(&maitre, &agent).await;
    assert_eq!(device_id_2, device_id);
    assert_eq!(zone_id_2, zone_id, "le nom et la zone sont gardés");
    let zone = zone_visible(&maitre, &device_id)
        .expect("un nouvel appairage doit faire revenir la zone supprimée");
    assert!(zone.online);
}

/// Un avis de révocation n'est pas cru sur parole : tant que l'agent accepte
/// le jeton du maître, l'avis ne supprime rien.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_faux_avis_de_revocation_ne_supprime_rien() {
    let (agent, _) = agent().await;
    let maitre = Serveur::demarrer().await;
    let (agent_id, _, device_id) = appaires(&maitre, &agent).await;
    let r = client()
        .post(maitre.url("/agent-tune/revocation"))
        .json(&json!({ "agent_id": agent_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert!(
        zone_visible(&maitre, &device_id).is_some(),
        "un avis non confirmé par l'agent ne doit rien supprimer"
    );
    assert!(maitre.state.outputs.lock().await.contains(&device_id));
}

/// Au démarrage du maître : un agent qui a révoqué le maître pendant son
/// absence fait supprimer ses zones ; un agent qui ne répond plus garde les
/// siennes, hors ligne.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn au_demarrage_une_revocation_supprime_les_zones_un_agent_muet_les_garde_hors_ligne() {
    let maitre = Serveur::demarrer().await;
    let (revoque, _) = agent().await;
    let (muet, _) = agent().await;
    let (_, _, device_revoque) = appaires(&maitre, &revoque).await;
    let (id_muet, _, device_muet) = appaires(&maitre, &muet).await;
    let (maitre_id, _) = tune_server::agent_tune::identite(&maitre.state);

    // Révocation sans avis au maître (il était éteint).
    assert!(
        tune_server::agent_tune::agent::oublier_maitre(&revoque.state, &maitre_id, false).await
    );
    // L'agent muet : le maître le cherche à une adresse où rien n'écoute.
    let ferme = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_ferme = ferme.local_addr().unwrap().port();
    drop(ferme);
    let mut liste = tune_server::agent_tune::maitre::agents(&maitre.state);
    for a in liste.iter_mut().filter(|a| a.agent_id == id_muet) {
        a.port = port_ferme;
    }
    SettingsRepo::with_backend(maitre.state.backend.clone())
        .set("agent_tune_agents", &serde_json::to_string(&liste).unwrap())
        .unwrap();

    tune_server::agent_tune::maitre::reinscrire_les_agents(&maitre.state).await;

    assert!(
        zone_visible(&maitre, &device_revoque).is_none(),
        "la zone d'un agent qui a révoqué le maître doit être supprimée au démarrage"
    );
    let zone_muette = zone_visible(&maitre, &device_muet)
        .expect("la zone d'un agent qui ne répond plus doit être gardée");
    assert!(
        !zone_muette.online,
        "la zone d'un agent qui ne répond plus doit passer hors ligne"
    );
    assert!(
        tune_server::agent_tune::maitre::agents(&maitre.state)
            .iter()
            .any(|a| a.agent_id == id_muet),
        "un agent muet reste appairé"
    );
}

// ---------------------------------------------------------------------------
// Plafond de zones gratuites
// ---------------------------------------------------------------------------

/// Une zone prêtée par un agent compte dans le plafond de zones gratuites du
/// MAÎTRE (décision du 08/10), pas chez l'agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_zone_d_agent_compte_dans_le_plafond_du_maitre_pas_chez_l_agent() {
    let une_zone = || tune_server::config::TuneConfig {
        free_max_zones: 1,
        ..Default::default()
    };

    // 1. Maître gratuit dont l'unique zone gratuite est déjà prise : la zone
    //    d'agent est refusée comme n'importe quelle nouvelle zone.
    let (agent_1, _) = agent().await;
    let plein = Serveur::demarrer_avec(une_zone()).await;
    let repo = ZoneRepo::with_backend(plein.state.backend.clone());
    let deja = repo
        .create("Salon", Some("dlna"), Some("uuid:salon"))
        .unwrap();
    repo.update_online(deja, true).unwrap();
    repo.save_playback_position(deja, 0, Some(1), Some("local"), None)
        .unwrap();
    let (_, zone_agent, _) = appaires(&plein, &agent_1).await;
    let r = client()
        .post(plein.url(&format!("/api/v1/zones/{zone_agent}/play")))
        .json(&json!({ "track_id": 1 }))
        .send()
        .await
        .unwrap();
    let code = r.status();
    let corps: Value = r.json().await.unwrap_or(Value::Null);
    assert_eq!(
        code, 402,
        "la zone d'agent doit compter dans le plafond du maître : {corps}"
    );
    assert_eq!(corps["code"], "free_zone_cap_reached", "{corps}");

    // 2. Maître gratuit libre : la zone d'agent joue et prend SA place chez
    //    le maître ; l'agent, gratuit lui aussi, ne la compte pas.
    let (agent_2, sortie_agent) = agent().await;
    let zones_agent = ZoneRepo::with_backend(agent_2.state.backend.clone());
    let zone_locale = zones_agent
        .create("DAC USB d'essai", Some("local"), Some(DEVICE_LOCAL))
        .unwrap();
    zones_agent.update_online(zone_locale, true).unwrap();
    let maitre = Serveur::demarrer_avec(une_zone()).await;
    let (_, zone_id, _) = appaires(&maitre, &agent_2).await;
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("piste.wav");
    std::fs::write(&chemin, wav(&pcm_de_reference())).unwrap();
    let mut t = tune_core::db::models::Track::new("Piste".into());
    t.file_path = Some(chemin.to_string_lossy().into_owned());
    t.format = Some("wav".into());
    t.sample_rate = Some(44_100);
    t.bit_depth = Some(16);
    t.channels = 2;
    t.duration_ms = 1_000;
    t.file_size = Some(std::fs::metadata(&chemin).unwrap().len() as i64);
    let track_id = tune_core::db::track_repo::TrackRepo::with_backend(maitre.state.backend.clone())
        .create(&t)
        .unwrap();
    let r = client()
        .post(maitre.url(&format!("/api/v1/zones/{zone_id}/play")))
        .json(&json!({ "track_id": track_id }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "lecture : {}", r.status());
    attendre(
        || {
            let e = sortie_agent.clone();
            async move { e.lectures.load(Ordering::SeqCst) > 0 }
        },
        "ordre de lecture reçu par l'agent",
    )
    .await;
    // La zone est ACTIVÉE au premier enregistrement de sa position : le
    // sondeur le fait périodiquement, une pause tout de suite. Le banc n'a pas
    // de sondeur, la pause de l'utilisateur en tient lieu.
    let r = client()
        .post(maitre.url(&format!("/api/v1/zones/{zone_id}/pause")))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "pause : {}", r.status());
    for _ in 0..200 {
        if maitre.state.license.plafond_zones().await.actives == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let zone_maitre = ZoneRepo::with_backend(maitre.state.backend.clone())
        .get(zone_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        maitre.state.license.plafond_zones().await.actives,
        1,
        "la zone d'agent doit consommer le quota du maître (en ligne : {}, dernière piste : {:?})",
        zone_maitre.online,
        zone_maitre.last_track_id
    );
    let chez_l_agent = agent_2.state.license.plafond_zones().await;
    assert_eq!(
        chez_l_agent.actives, 0,
        "la zone d'agent ne doit rien consommer chez l'agent : {chez_l_agent:?}"
    );
    assert!(!chez_l_agent.atteint());
    assert!(
        zones_agent
            .get(zone_locale)
            .unwrap()
            .unwrap()
            .last_track_id
            .is_none(),
        "la zone locale de l'agent ne doit pas être activée par le maître"
    );
}
