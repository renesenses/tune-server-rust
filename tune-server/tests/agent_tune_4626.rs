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
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = ecoute.local_addr().unwrap().port();
        let state = AppState::new(":memory:", port, Default::default()).unwrap();
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
    let mut graine: u32 = 0x4626_4626;
    let mut pcm = Vec::with_capacity(44_100 * 4);
    for _ in 0..44_100 * 2 {
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
}

impl Default for Enregistreuse {
    fn default() -> Self {
        Self {
            recu: Arc::default(),
            etat: Arc::new(Mutex::new((TransportState::Stopped, None))),
            volume: Arc::default(),
            muet: Arc::default(),
            lectures: Arc::default(),
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
        tokio::spawn(async move {
            if let Ok(r) = client().get(&url).send().await
                && let Ok(octets) = r.bytes().await
            {
                e.recu.lock().await.extend_from_slice(&octets);
            }
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
    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
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
    let debut = recu
        .windows(4)
        .position(|w| w == b"data")
        .map(|p| p + 8)
        .unwrap_or(0);
    assert!(
        recu[debut..].starts_with(&pcm),
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
    let chemin = dossier.path().join("piste.wav");
    let pcm = pcm_de_reference();
    std::fs::write(&chemin, wav(&pcm)).unwrap();
    let pistes = tune_core::db::track_repo::TrackRepo::with_backend(maitre.state.backend.clone());
    let mut t = tune_core::db::models::Track::new("Piste de bibliothèque".into());
    t.file_path = Some(chemin.to_string_lossy().into_owned());
    t.format = Some("wav".into());
    t.sample_rate = Some(44_100);
    t.bit_depth = Some(16);
    t.channels = 2;
    t.duration_ms = 1_000;
    t.file_size = Some(std::fs::metadata(&chemin).unwrap().len() as i64);
    let track_id = pistes.create(&t).unwrap();

    let r = client()
        .post(maitre.url(&format!("/api/v1/zones/{zone_id}/play")))
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
    let debut = recu
        .windows(4)
        .position(|w| w == b"data")
        .map(|p| p + 8)
        .unwrap_or(0);
    assert!(
        recu[debut..].starts_with(&pcm),
        "échantillons altérés entre la bibliothèque du maître et l'agent \
         ({} octets reçus depuis {url})",
        recu.len()
    );
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
    let (maitre_id, _) = tune_server::agent_tune::identite(&maitre.state);
    let r = client()
        .delete(agent.url(&format!("/api/v1/agent-tune/agent/maitres/{maitre_id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let sortie = sortie_du_maitre(&maitre, &device_id).await;
    let refus = sortie.lock().await.set_volume(0.5).await.unwrap_err();
    assert!(refus.contains("401"), "{refus}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn le_maitre_qui_oublie_l_agent_retire_ses_sorties_et_garde_la_zone_hors_ligne() {
    let (agent, _) = agent().await;
    let maitre = Serveur::demarrer().await;
    let (agent_id, zone_id, device_id) = appaires(&maitre, &agent).await;
    let r = client()
        .delete(maitre.url(&format!("/api/v1/agent-tune/agents/{agent_id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(!maitre.state.outputs.lock().await.contains(&device_id));
    let zone = ZoneRepo::with_backend(maitre.state.backend.clone())
        .get_by_device_id(&device_id)
        .unwrap()
        .expect("la zone est gardée");
    assert_eq!(zone.id, Some(zone_id));
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
