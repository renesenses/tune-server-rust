//! #5575 (Tades, fils 2071 et 2482) — la zone DLNA « DDC-0 C19 » sort sur un
//! Diretta UPnP Renderer, dont le `SetVolume` est décoratif : acquitté, relu
//! fidèlement, jamais appliqué au son (`dlna_volume_decoratif_5575.rs`).
//!
//! Depuis la rc8 (#7016), Tune refusait chaque consigne sous 100 %, mais la
//! sortie déclarait toujours `can_set_volume: true` : le client gardait un
//! curseur actif, et chaque geste finissait en `zone.playback_error` (les
//! « deux messages d'erreur » du fil 2482). La sortie déclare désormais
//! `can_set_volume: false` : le client désactive le curseur
//! (`volumeCommandable`, web) au lieu d'accumuler des refus.
//!
//! Le banc : un VRAI `DlnaOutput` branché sur un faux renderer (axum) qui
//! acquitte tout et compte les `SetVolume` reçus. Aucun son.
//!
//! Contre-épreuve : rendre `true` en dur à `can_set_volume` dans
//! `DlnaOutput::capabilities` fait tomber les deux tests (capacité annoncée,
//! puis armement qui envoie un `SetVolume` à l'appareil).
use super::PlaybackOrchestrator;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::OutputTarget;
use crate::outputs::dlna::DlnaOutput;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::ServiceRegistry;
use axum::http::{HeaderMap, StatusCode};
use axum::{Router, routing::post};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;
use tune_output_api::OutputCommandError;

/// L'UDN par défaut du projet Diretta, celui du journal de Tades.
const DIRETTA: &str = "uuid:diretta-renderer-a7366917f9ddf66e";
const ORDINAIRE: &str = "uuid:renderer-ordinaire-5575";

/// Un faux renderer qui acquitte tout ; rend son adresse et le compteur de
/// `SetVolume` reçus.
async fn faux_renderer() -> (String, Arc<AtomicU64>, tokio::task::JoinHandle<()>) {
    let compteur = Arc::new(AtomicU64::new(0));
    let c = compteur.clone();
    let app = Router::new().route(
        "/control",
        post(move |headers: HeaderMap| {
            let c = c.clone();
            async move {
                let action = headers
                    .get("SOAPAction")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                if action.contains("#SetVolume") {
                    c.fetch_add(1, Ordering::SeqCst);
                }
                (StatusCode::OK, "<u:Response/>".to_owned())
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hote = format!("http://{}", listener.local_addr().unwrap());
    let serveur = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (hote, compteur, serveur)
}

fn sortie(hote: &str, udn: &str) -> DlnaOutput {
    DlnaOutput::new(
        "C19".into(),
        udn.into(),
        hote.into(),
        format!("{hote}/control"),
        format!("{hote}/control"),
        None,
    )
}

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

#[tokio::test]
async fn un_volume_decoratif_s_annonce_non_reglable_5575() {
    let (hote, _, serveur) = faux_renderer().await;
    let par_udn = sortie(&hote, DIRETTA).capabilities();
    assert!(
        !par_udn.can_set_volume,
        "#5575 : un Diretta UPnP Renderer annonce un volume réglable ; le \
         client garde un curseur actif dont chaque geste est refusé (fil 2482)"
    );
    // Le même appareil sous un UDN choisi (`--uuid`), reconnu par son modelName.
    let par_modele = sortie(&hote, "uuid:choisi-a-la-main")
        .with_modele(Some("Diretta UPnP Renderer".into()))
        .capabilities();
    assert!(!par_modele.can_set_volume, "reconnu par son modelName");
    // Le reste des capacités ne bouge pas : lecture, pause, avance, sourdine.
    assert!(par_udn.can_pause && par_udn.can_resume && par_udn.can_seek && par_udn.can_mute);
    // Témoin : un renderer ordinaire garde son volume réglable.
    assert!(sortie(&hote, ORDINAIRE).capabilities().can_set_volume);
    serveur.abort();
}

#[tokio::test]
async fn sur_un_volume_decoratif_le_curseur_est_refuse_mais_le_volume_fixe_s_arme_5575() {
    let (hote, compteur, serveur) = faux_renderer().await;
    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("C19", Some("dlna"), Some(DIRETTA))
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(sortie(&hote, DIRETTA)));

    // Une consigne à 3 % (le curseur du fil 2071) : refusée, rien ne part.
    let refus = orch
        .set_volume(zone_id, 0.03, Some(DIRETTA))
        .await
        .expect_err("#5575 : une consigne sur un volume décoratif est acceptée");
    assert!(
        matches!(refus, OutputCommandError::Unsupported { .. }),
        "refus par capacité, avant tout envoi : {refus}"
    );
    assert_eq!(compteur.load(Ordering::SeqCst), 0, "aucun SetVolume envoyé");

    // Le volume fixe, que le message de refus conseille, doit pouvoir
    // s'armer : l'appareil est déjà à son plein niveau, rien à lui commander.
    orch.arm_fixed_volume(zone_id, Some(DIRETTA))
        .await
        .expect("#5575 : le volume fixe ne s'arme plus sur un volume décoratif");
    assert_eq!(
        compteur.load(Ordering::SeqCst),
        0,
        "l'armement ne commande rien à un volume décoratif"
    );
    assert_eq!(orch.playback.get_state(zone_id).await.volume, 1.0);

    // Témoin : sur un renderer ordinaire, l'armement commande bien 100 %.
    let zone_ordinaire = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), Some(ORDINAIRE))
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(sortie(&hote, ORDINAIRE)));
    orch.arm_fixed_volume(zone_ordinaire, Some(ORDINAIRE))
        .await
        .expect("renderer ordinaire : armement");
    assert!(
        compteur.load(Ordering::SeqCst) >= 1,
        "témoin : l'armement d'un renderer ordinaire envoie son SetVolume"
    );
    serveur.abort();
}

/// Relecture du 11/11 : avec `can_set_volume: false`, le contrôle de capacité
/// refusait AUSSI les consignes à 100 % qui passaient avant (PURE forcé,
/// restauration, réveil) : PURE devenait inactivable sur un Diretta (422).
/// Une consigne pleine est tenue sans commande ; une consigne plus basse
/// reste refusée.
#[tokio::test]
async fn sur_un_volume_decoratif_pure_force_et_100_pour_cent_passent_sans_commande_5575() {
    let (hote, compteur, serveur) = faux_renderer().await;
    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("C19", Some("dlna"), Some(DIRETTA))
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(sortie(&hote, DIRETTA)));

    let envoi = orch
        .set_volume_pure_force(zone_id, Some(DIRETTA))
        .await
        .expect("#5575 : PURE forcé à 100 % refusé sur un volume décoratif");
    assert!(!envoi.envoye(), "aucune commande : {envoi:?}");
    orch.set_volume(zone_id, 1.0, Some(DIRETTA))
        .await
        .expect("#5575 : une consigne à 100 % refusée sur un volume décoratif");
    assert_eq!(compteur.load(Ordering::SeqCst), 0, "aucun SetVolume envoyé");
    assert_eq!(orch.playback.get_state(zone_id).await.volume, 1.0);
    // Une consigne plus basse reste refusée.
    assert!(orch.set_volume(zone_id, 0.5, Some(DIRETTA)).await.is_err());
    serveur.abort();
}
