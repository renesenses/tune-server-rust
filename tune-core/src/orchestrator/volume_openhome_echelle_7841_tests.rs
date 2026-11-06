//! #7841 — Linn Klimax en OpenHome, serveur Windows : « à 30, plus de son ».
//!
//! Tune envoyait `SetVolume round(volume × 100)` sans jamais lire l'échelle
//! de l'appareil (action `Characteristics` du service `Volume:1`). Sur un
//! renderer dont `VolumeMax` n'est pas 100, le niveau envoyé n'est pas celui
//! que le curseur demande, et la lecture en retour (`Volume`) est fausse du
//! même facteur. La sourdine, elle, attendait encore la fin des relevés du
//! sondeur, comme le volume avant #7633.
//!
//! Le banc : une vraie `OpenHomeOutput` contre un faux Linn qui publie
//! l'échelle choisie par le test, et dont les relevés sont lents. La question
//! qui vaille : « qu'a reçu le Linn ? ».
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use tokio::sync::Mutex;

use super::PlaybackOrchestrator;
use crate::audio::garde_de_hausse::Hausse;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::openhome::OpenHomeOutput;
use crate::outputs::registry::OutputRegistry;
use crate::outputs::traits::OutputTarget;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

const ID: &str = "uuid:linn-klimax-7841";

/// Ce que le faux Linn publie et ce qu'il a reçu.
struct Linn {
    /// `Some((max, unity, steps, mdb))` : la réponse à `Characteristics`.
    /// `None` : une faute SOAP, comme un renderer sans cette action.
    echelle: Option<(u32, u32, u32, u32)>,
    /// La réponse aux relevés `Volume`.
    niveau_releve: u32,
    /// La durée des relevés (`Time`, `Volume`, `Mute`).
    releve: Duration,
    caracteristiques_lues: AtomicUsize,
    volumes: StdMutex<Vec<u32>>,
    sourdines: StdMutex<Vec<String>>,
    releve_en_cours: AtomicBool,
}

fn valeur(corps: &str) -> Option<String> {
    crate::outputs::openhome::extract_tag(corps, "Value")
}

fn faute() -> (StatusCode, String) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "<s:Envelope><s:Body><s:Fault><faultcode>s:Client</faultcode>\
         <faultstring>UPnPError</faultstring><detail><UPnPError>\
         <errorCode>401</errorCode><errorDescription>Invalid Action\
         </errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"
            .to_string(),
    )
}

async fn faux_linn(linn: Linn) -> (u16, Arc<Linn>) {
    let linn = Arc::new(linn);
    let pour_time = linn.clone();
    let pour_volume = linn.clone();
    let app = Router::new()
        .route(
            "/Time",
            post(move || {
                let linn = pour_time.clone();
                async move {
                    linn.releve_en_cours.store(true, Ordering::SeqCst);
                    tokio::time::sleep(linn.releve).await;
                    "<s:Envelope><s:Body><u:TimeResponse><TrackCount>1</TrackCount>\
                     <TrackDuration>300</TrackDuration><Seconds>42</Seconds>\
                     </u:TimeResponse></s:Body></s:Envelope>"
                        .to_string()
                }
            }),
        )
        .route(
            "/Volume",
            post(move |entetes: HeaderMap, corps: String| {
                let linn = pour_volume.clone();
                async move {
                    let action = entetes
                        .get("SOAPAction")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    let ok = |corps: String| (StatusCode::OK, corps);
                    if action.contains("#Characteristics") {
                        linn.caracteristiques_lues.fetch_add(1, Ordering::SeqCst);
                        let Some((max, unity, steps, mdb)) = linn.echelle else {
                            return faute();
                        };
                        return ok(format!(
                            "<s:Envelope><s:Body><u:CharacteristicsResponse>\
                             <VolumeMax>{max}</VolumeMax><VolumeUnity>{unity}</VolumeUnity>\
                             <VolumeSteps>{steps}</VolumeSteps>\
                             <VolumeMilliDbPerStep>{mdb}</VolumeMilliDbPerStep>\
                             <BalanceMax>15</BalanceMax><FadeMax>15</FadeMax>\
                             </u:CharacteristicsResponse></s:Body></s:Envelope>"
                        ));
                    }
                    if action.contains("#SetVolume") {
                        let v = valeur(&corps)
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(u32::MAX);
                        linn.volumes.lock().unwrap().push(v);
                        return ok(
                            "<s:Envelope><s:Body><u:SetVolumeResponse/></s:Body></s:Envelope>"
                                .into(),
                        );
                    }
                    if action.contains("#SetMute") {
                        linn.sourdines
                            .lock()
                            .unwrap()
                            .push(valeur(&corps).unwrap_or_default());
                        return ok(
                            "<s:Envelope><s:Body><u:SetMuteResponse/></s:Body></s:Envelope>".into(),
                        );
                    }
                    // `Volume` et `Mute` : les relevés du sondeur, lents.
                    tokio::time::sleep(linn.releve).await;
                    let niveau = if action.contains("#Mute") {
                        0
                    } else {
                        linn.niveau_releve
                    };
                    ok(format!(
                        "<s:Envelope><s:Body><u:Response><Value>{niveau}</Value></u:Response>\
                         </s:Body></s:Envelope>"
                    ))
                }
            }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    (port, linn)
}

fn linn(echelle: Option<(u32, u32, u32, u32)>) -> Linn {
    Linn {
        echelle,
        niveau_releve: 0,
        releve: Duration::from_millis(10),
        caracteristiques_lues: AtomicUsize::new(0),
        volumes: StdMutex::new(Vec::new()),
        sourdines: StdMutex::new(Vec::new()),
        releve_en_cours: AtomicBool::new(false),
    }
}

fn sortie(port: u16) -> OpenHomeOutput {
    let mut services = HashMap::new();
    services.insert("time".to_string(), "/Time".to_string());
    services.insert("volume".to_string(), "/Volume".to_string());
    OpenHomeOutput::new(
        "Linn Klimax".into(),
        ID.into(),
        "127.0.0.1".into(),
        port,
        services,
        None,
        HashMap::new(),
    )
}

struct Banc {
    orch: Arc<PlaybackOrchestrator>,
    zone_id: i64,
    linn: Arc<Linn>,
}

/// Une zone OpenHome à 50 % sur le faux Linn, sans abonnement aux évènements
/// (le cas du serveur Windows derrière son pare-feu).
async fn banc(linn: Linn) -> Banc {
    let (port, linn) = faux_linn(linn).await;
    let mut registre = OutputRegistry::new();
    registre.register(Box::new(sortie(port)));
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(registre)),
        None,
    );
    let repo = ZoneRepo::with_backend(db);
    let zone_id = repo.create("Klimax", Some("openhome"), Some(ID)).unwrap();
    repo.update_volume(zone_id, 50.0).unwrap();
    orch.playback.set_volume(zone_id, 0.5).await;
    Banc {
        orch: Arc::new(orch),
        zone_id,
        linn,
    }
}

async fn volume(banc: &Banc, v: f64) {
    banc.orch
        .set_volume_avec_garde(banc.zone_id, v, Some(ID), Hausse::Confirmee)
        .await
        .map(|_| ())
        .unwrap();
}

/// 🔴 Le témoin de l'échelle. Rouge avant : le Linn qui publie `VolumeMax`
/// 50 recevait `SetVolume 30` pour 30 %, soit 60 % de son échelle.
#[tokio::test(flavor = "multi_thread")]
async fn a_30_pour_cent_un_linn_a_volume_max_50_recoit_set_volume_15() {
    let banc = banc(linn(Some((50, 40, 50, 1000)))).await;

    volume(&banc, 0.3).await;
    volume(&banc, 1.0).await;

    assert_eq!(
        *banc.linn.volumes.lock().unwrap(),
        vec![15, 50],
        "#7841 : le niveau envoyé doit suivre l'échelle publiée par le Linn \
         (VolumeMax 50), pas une échelle 0..100 supposée"
    );
    assert_eq!(
        banc.linn.caracteristiques_lues.load(Ordering::SeqCst),
        1,
        "l'échelle se lit une fois, pas à chaque commande"
    );
}

/// Un renderer sans `Characteristics` (faute SOAP) garde l'échelle 0..100 :
/// rien ne change pour lui.
#[tokio::test(flavor = "multi_thread")]
async fn sans_characteristics_l_echelle_0_100_reste() {
    let banc = banc(linn(None)).await;

    volume(&banc, 0.3).await;
    volume(&banc, 0.25).await;

    assert_eq!(*banc.linn.volumes.lock().unwrap(), vec![30, 25]);
    assert_eq!(
        banc.linn.caracteristiques_lues.load(Ordering::SeqCst),
        1,
        "une faute SOAP vaut réponse : l'action n'est pas redemandée à chaque commande"
    );
}

/// 🔴 L'échelle est écrite au journal, avec ce que vaut le niveau en dB :
/// c'est ce que le rapport du testeur montrera. Rouge avant : aucun
/// `oh_volume_characteristics`.
#[tokio::test]
async fn l_echelle_lue_est_journalisee_avec_l_ecart_au_gain_unite() {
    let (port, _linn) = faux_linn(linn(Some((100, 80, 100, 1000)))).await;
    let envoi = sortie(port).envoi_de_volume().unwrap();
    let journal = Journal::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    {
        let _garde = tracing::subscriber::set_default(abonne);
        envoi.set_volume(0.3).await.unwrap();
    }
    let texte = journal.texte();
    let ligne = texte
        .lines()
        .find(|l| l.contains("oh_volume_characteristics"))
        .unwrap_or_else(|| {
            panic!("#7841 : oh_volume_characteristics absent du journal :\n{texte}")
        });
    for attendu in [
        "volume_max=100",
        "volume_unity=80",
        "volume_steps=100",
        "volume_milli_db_per_step=1000",
    ] {
        assert!(ligne.contains(attendu), "{attendu} absent de : {ligne}");
    }
    // 30 sur un Linn dont le gain unité est à 80, 1 dB par cran : −50 dB.
    let envoi_ligne = texte
        .lines()
        .find(|l| l.contains("oh_set_volume"))
        .unwrap_or_else(|| panic!("oh_set_volume absent du journal :\n{texte}"));
    assert!(envoi_ligne.contains("level=30"), "{envoi_ligne}");
    assert!(
        envoi_ligne.contains("db_relative_to_unity=-50"),
        "{envoi_ligne}"
    );
}

/// 🔴 Le relevé se lit sur l'échelle du Linn. Rouge avant : un niveau 15 sur
/// `VolumeMax` 50 remontait comme 15 %, et le sondeur voyait un écart.
#[tokio::test(flavor = "multi_thread")]
async fn le_releve_du_linn_se_lit_sur_son_echelle() {
    let mut l = linn(Some((50, 40, 50, 1000)));
    l.niveau_releve = 15;
    let (port, _linn) = faux_linn(l).await;

    let statut = sortie(port).get_status().await.unwrap();

    assert!(
        (statut.volume - 0.3).abs() < 1e-9,
        "#7841 : le niveau 15 d'un Linn à VolumeMax 50 vaut 30 %, pas {}",
        statut.volume
    );
}

/// 🔴 La sourdine part sans attendre le relevé du sondeur, comme le volume
/// depuis #7633. Rouge avant : `SetMute` attendait la fin des relevés
/// (environ 4,5 s ici).
#[tokio::test(flavor = "multi_thread")]
async fn la_sourdine_openhome_part_sans_attendre_le_releve_du_sondeur() {
    let mut l = linn(Some((100, 80, 100, 1000)));
    l.releve = Duration::from_millis(1500);
    let banc = banc(l).await;
    let sortie = banc.orch.outputs.lock().await.get(ID).unwrap();
    let releve = tokio::spawn(async move {
        let sortie = sortie.lock().await;
        let _ = sortie.get_status().await;
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !banc.linn.releve_en_cours.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("le faux Linn n'a pas reçu le relevé Time du sondeur");

    let debut = Instant::now();
    banc.orch
        .set_mute(banc.zone_id, true, Some(ID))
        .await
        .unwrap();
    let duree = debut.elapsed();

    assert!(
        duree < Duration::from_millis(1000),
        "#7841 : le SetMute OpenHome a attendu le relevé du sondeur ({duree:?})"
    );
    assert!(
        !releve.is_finished(),
        "le relevé du sondeur devait encore tenir la sortie pendant l'envoi"
    );
    assert_eq!(*banc.linn.sourdines.lock().unwrap(), vec!["1".to_string()]);
    assert!(banc.orch.playback.get_state(banc.zone_id).await.muted);
    releve.await.unwrap();
}

#[derive(Clone, Default)]
struct Journal(Arc<StdMutex<Vec<u8>>>);

impl Journal {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for Journal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Journal;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
