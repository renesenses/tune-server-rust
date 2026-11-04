//! #7633 — Linn Klimax en OpenHome, serveur Windows : un changement de volume
//! mettait 1 à 2 s à s'appliquer, et après 70 le 50 restait « trop fort ».
//!
//! Le sondeur tient le verrou de la sortie pendant tout `get_status`. Sans
//! évènements du renderer (pare-feu Windows, #4677), ce sont des appels SOAP
//! à la suite (`Time`, `Volume`, `Mute`) ; la consigne de volume attendait
//! derrière eux, et le Linn jouait encore l'ancienne valeur.
//!
//! Le banc : une vraie `OpenHomeOutput` enregistrée contre un faux Linn dont
//! les relevés répondent en 1,5 s et dont `SetVolume` répond tout de suite.
//! La question qui vaille : « qu'a reçu le Linn, et quand ? ».
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

const ID: &str = "uuid:linn-klimax-7633";
const RELEVE_LENT: Duration = Duration::from_millis(1500);

/// Ce que le faux Linn a reçu.
#[derive(Default)]
struct Linn {
    /// Les `SetVolume` acceptés, dans l'ordre d'arrivée.
    volumes: std::sync::Mutex<Vec<u32>>,
    /// Un relevé `Time` est en cours : le sondeur tient la sortie.
    releve_en_cours: AtomicBool,
    /// La valeur que le Linn refuse par une faute SOAP.
    refuse: Option<u32>,
}

fn valeur(corps: &str) -> Option<u32> {
    crate::outputs::openhome::extract_tag(corps, "Value")?
        .parse()
        .ok()
}

async fn faux_linn(refuse: Option<u32>) -> (u16, Arc<Linn>) {
    let linn = Arc::new(Linn {
        refuse,
        ..Default::default()
    });
    let pour_time = linn.clone();
    let pour_volume = linn.clone();
    let app = Router::new()
        .route(
            "/Time",
            post(move || {
                let linn = pour_time.clone();
                async move {
                    linn.releve_en_cours.store(true, Ordering::SeqCst);
                    tokio::time::sleep(RELEVE_LENT).await;
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
                    if action.contains("#SetVolume") {
                        let v = valeur(&corps).unwrap_or(u32::MAX);
                        if linn.refuse == Some(v) {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "<s:Envelope><s:Body><s:Fault><faultcode>s:Client</faultcode>\
                                 <faultstring>UPnPError</faultstring><detail><UPnPError>\
                                 <errorCode>501</errorCode><errorDescription>Action Failed\
                                 </errorDescription></UPnPError></detail></s:Fault>\
                                 </s:Body></s:Envelope>"
                                    .to_string(),
                            );
                        }
                        linn.volumes.lock().unwrap().push(v);
                        return (
                            StatusCode::OK,
                            "<s:Envelope><s:Body><u:SetVolumeResponse/></s:Body></s:Envelope>"
                                .to_string(),
                        );
                    }
                    // `Volume` et `Mute` : les relevés du sondeur, lents.
                    tokio::time::sleep(RELEVE_LENT).await;
                    (
                        StatusCode::OK,
                        "<s:Envelope><s:Body><u:Response><Value>0</Value></u:Response>\
                         </s:Body></s:Envelope>"
                            .to_string(),
                    )
                }
            }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    (port, linn)
}

struct Banc {
    orch: Arc<PlaybackOrchestrator>,
    zone_id: i64,
    linn: Arc<Linn>,
}

/// Une zone OpenHome à 30 % sur le faux Linn, sans abonnement aux évènements
/// (le cas du serveur Windows derrière son pare-feu).
async fn banc(refuse: Option<u32>) -> Banc {
    let (port, linn) = faux_linn(refuse).await;
    let mut services = HashMap::new();
    services.insert("time".to_string(), "/Time".to_string());
    services.insert("volume".to_string(), "/Volume".to_string());
    let mut registre = OutputRegistry::new();
    registre.register(Box::new(OpenHomeOutput::new(
        "Linn Klimax".into(),
        ID.into(),
        "127.0.0.1".into(),
        port,
        services,
        None,
        HashMap::new(),
    )));
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
    repo.update_volume(zone_id, 30.0).unwrap();
    orch.playback.set_volume(zone_id, 0.3).await;
    Banc {
        orch: Arc::new(orch),
        zone_id,
        linn,
    }
}

/// Le sondeur en plein relevé : il tient la sortie pendant tout `get_status`
/// (`poller.rs`), et le relevé `Time` est commencé au retour.
async fn sondeur_en_releve(banc: &Banc) -> tokio::task::JoinHandle<()> {
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
    releve
}

async fn volume(banc: &Banc, v: f64) -> Result<(), crate::outputs::OutputCommandError> {
    banc.orch
        .set_volume_avec_garde(banc.zone_id, v, Some(ID), Hausse::Confirmee)
        .await
        .map(|_| ())
}

/// 🔴 Le témoin de #7633. Rouge avant : chaque `SetVolume` attendait la fin
/// du relevé du sondeur (environ 4,5 s ici, trois appels lents à la suite) ;
/// 70 puis 50 n'atteignaient le Linn qu'après lui.
#[tokio::test(flavor = "multi_thread")]
async fn le_volume_openhome_part_sans_attendre_le_releve_du_sondeur() {
    let banc = banc(None).await;
    let releve = sondeur_en_releve(&banc).await;

    let debut = Instant::now();
    volume(&banc, 0.7).await.unwrap();
    volume(&banc, 0.5).await.unwrap();
    let duree = debut.elapsed();

    assert!(
        duree < Duration::from_millis(1000),
        "#7633 : le SetVolume OpenHome a attendu le relevé du sondeur ({duree:?}) — \
         le Linn joue encore l'ancienne valeur pendant ce temps"
    );
    assert!(
        !releve.is_finished(),
        "le relevé du sondeur devait encore tenir la sortie pendant l'envoi"
    );
    assert_eq!(
        *banc.linn.volumes.lock().unwrap(),
        vec![70, 50],
        "le Linn doit recevoir les consignes dans l'ordre, la dernière en dernier"
    );
    releve.await.unwrap();
    // Le relevé terminé ne renvoie rien au Linn : 50 reste la dernière valeur.
    assert_eq!(banc.linn.volumes.lock().unwrap().last(), Some(&50));
    assert!((banc.orch.playback.get_state(banc.zone_id).await.volume - 0.5).abs() < 1e-9);
}

/// 🔴 Une faute SOAP du Linn n'est pas une réussite. Rouge avant : le 500
/// passait pour accepté, la mémoire gardait 50 et aucune erreur ne partait.
#[tokio::test(flavor = "multi_thread")]
async fn un_set_volume_refuse_par_le_linn_est_une_erreur_et_la_valeur_acceptee_revient() {
    let banc = banc(Some(50)).await;
    volume(&banc, 0.7).await.unwrap();

    let refus = volume(&banc, 0.5).await;

    assert!(
        refus.is_err(),
        "#7633 : la faute SOAP du Linn sur SetVolume 50 est passée pour une réussite"
    );
    assert_eq!(*banc.linn.volumes.lock().unwrap(), vec![70]);
    let memoire = banc.orch.playback.get_state(banc.zone_id).await.volume;
    assert!(
        (memoire - 0.7).abs() < 1e-9,
        "la mémoire doit revenir à la dernière valeur acceptée (0,7), pas {memoire}"
    );
}

/// L'envoi direct n'existe que pour un renderer qui publie son service
/// Volume ; les autres gardent le chemin sous verrou.
#[test]
fn seul_un_renderer_avec_service_volume_a_un_envoi_direct() {
    let sans_volume = OpenHomeOutput::new(
        "Sans volume".into(),
        "uuid:sans-volume".into(),
        "127.0.0.1".into(),
        1,
        HashMap::new(),
        None,
        HashMap::new(),
    );
    assert!(sans_volume.envoi_de_volume().is_none());
    let mut registre = OutputRegistry::new();
    registre.register(Box::new(sans_volume));
    assert!(registre.envoi_de_volume("uuid:sans-volume").is_none());
    registre.register(Box::new(
        crate::outputs::mock::MockOutput::new("dlna-1", "DLNA").with_type("dlna"),
    ));
    assert!(registre.envoi_de_volume("dlna-1").is_none());
}
