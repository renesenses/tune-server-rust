//! 🔴 #6721 / #7119 — témoins du repère de la garde contre les montées
//! brusques, vus depuis la SORTIE FACTICE : « qu'a reçu l'appareil ? ».
//!
//! La mémoire de la zone est à 80 %, l'appareil joue à 10 % (baissé hors de
//! Tune, relevé par le sondeur sans être adopté). L'auditeur saisit le curseur
//! pour BAISSER, à 60 % : l'appareil ne doit pas sauter de 10 à 60.
use std::sync::Arc;

use tokio::sync::Mutex;

use super::PlaybackOrchestrator;
use crate::audio::garde_de_hausse::{Hausse, SEUIL_DE_HAUSSE_DB};
use crate::audio::volume_scale::PLAGE_PERCEPTIVE_DB;
use crate::db::migrations::run_migrations;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::mock::MockOutput;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

const APPAREIL: &str = "dlna-marantz-repere-6721";

/// Une zone sur une sortie factice de type `type_sortie`, au volume `depart`
/// en base ET en mémoire (ce que fait `restore_zone_volumes` au démarrage).
async fn zone(type_sortie: &str, depart: f64) -> (Arc<PlaybackOrchestrator>, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let orch = Arc::new(PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    ));
    let repo = ZoneRepo::with_backend(db.clone());
    let zone_id = repo
        .create("Marantz NR1200", Some(type_sortie), Some(APPAREIL))
        .unwrap();
    repo.update_volume(zone_id, depart * 100.0).unwrap();
    orch.playback.set_volume(zone_id, depart).await;
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Marantz NR1200").with_type(type_sortie),
    ));
    (orch, zone_id)
}

async fn recus(orch: &PlaybackOrchestrator) -> Vec<f64> {
    let outputs = orch.outputs.lock().await;
    let out = outputs.get(APPAREIL).expect("sortie enregistrée");
    let guard = out.lock().await;
    guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("la sortie factice")
        .volume_calls()
        .await
}

fn en_base(orch: &PlaybackOrchestrator, zone_id: i64) -> f64 {
    ZoneRepo::with_backend(orch.db.clone())
        .get(zone_id)
        .unwrap()
        .unwrap()
        .volume
        / 100.0
}

/// Sur un appareil réseau, le plafond est 16,7 points de curseur au-dessus.
const POINTS_DU_SEUIL: f64 = SEUIL_DE_HAUSSE_DB / PLAGE_PERCEPTIVE_DB;

/// LE geste : mémoire à 80 %, appareil à 10 %, curseur saisi pour baisser à
/// 60 %. Sans repère sur l'appareil, 60 % partait tel quel (+50 points).
#[tokio::test]
async fn baisser_le_curseur_ne_fait_pas_sauter_un_appareil_plus_bas_que_la_memoire_6721() {
    let (orch, zone_id) = zone("dlna", 0.80).await;
    orch.niveaux_des_appareils.noter(zone_id, 0.10);
    let retenu = orch
        .set_volume_avec_garde(zone_id, 0.60, Some(APPAREIL), Hausse::Gardee)
        .await
        .unwrap();
    let recu = recus(&orch).await;
    let plafond = 0.10 + POINTS_DU_SEUIL;
    assert!(
        !recu.is_empty() && recu.iter().all(|v| *v <= plafond + 1e-9),
        "#6721 : l'appareil jouait à 10 % (mémoire de la zone à 80 %) ; un geste pour \
         BAISSER le curseur à 60 % lui a envoyé {recu:?} — saut de plus de \
         {SEUIL_DE_HAUSSE_DB} dB perçus depuis le niveau réel"
    );
    assert!(
        retenu.hausse_plafonnee,
        "le client doit pouvoir confirmer : {retenu:?}"
    );
    assert!((en_base(&orch, zone_id) - plafond).abs() < 1e-9);
}

/// La même chose par un pas « volume − » calculé depuis la mémoire.
#[tokio::test]
async fn un_pas_depuis_la_memoire_ne_fait_pas_sauter_l_appareil_6721() {
    let (orch, zone_id) = zone("dlna", 0.80).await;
    orch.niveaux_des_appareils.noter(zone_id, 0.05);
    orch.set_volume_au_pas(zone_id, 0.75, Some(APPAREIL))
        .await
        .unwrap();
    let recu = recus(&orch).await;
    assert!(
        recu.iter().all(|v| *v <= 0.05 + POINTS_DU_SEUIL + 1e-9),
        "#6721 : un pas « volume − » depuis 80 % (appareil à 5 %) a envoyé {recu:?}"
    );
}

/// Une confirmation explicite passe, comme avant.
#[tokio::test]
async fn une_hausse_confirmee_passe_depuis_le_niveau_reel_6721() {
    let (orch, zone_id) = zone("dlna", 0.80).await;
    orch.niveaux_des_appareils.noter(zone_id, 0.10);
    orch.set_volume_avec_garde(zone_id, 0.60, Some(APPAREIL), Hausse::Confirmee)
        .await
        .unwrap();
    assert_eq!(recus(&orch).await, vec![0.60]);
}

/// Un appareil PLUS HAUT que la mémoire (remonté de lui-même, #6008) ne
/// desserre pas la garde : le plafond reste mesuré depuis la mémoire.
#[tokio::test]
async fn un_appareil_plus_haut_ne_desserre_pas_la_garde_6721() {
    let (orch, zone_id) = zone("dlna", 0.30).await;
    orch.niveaux_des_appareils.noter(zone_id, 0.95);
    orch.set_volume(zone_id, 1.0, Some(APPAREIL)).await.unwrap();
    let recu = recus(&orch).await;
    assert!(
        recu.iter().all(|v| *v <= 0.30 + POINTS_DU_SEUIL + 1e-9),
        "un relevé à 95 % ne doit pas élargir la garde : {recu:?}"
    );
}

/// Un glissement ordinaire n'est pas gêné : chaque consigne acceptée devient
/// le niveau connu de l'appareil.
#[tokio::test]
async fn un_glissement_par_petits_pas_passe_en_entier_6721() {
    let (orch, zone_id) = zone("dlna", 0.10).await;
    orch.niveaux_des_appareils.noter(zone_id, 0.10);
    for v in [0.20, 0.30, 0.40, 0.50] {
        orch.set_volume(zone_id, v, Some(APPAREIL)).await.unwrap();
    }
    assert_eq!(recus(&orch).await, vec![0.20, 0.30, 0.40, 0.50]);
    assert_eq!(orch.niveaux_des_appareils.connu(zone_id), Some(0.50));
}

/// La limite de confort se mesure aussi depuis le niveau réel : mémoire à
/// 80 % au-dessus d'un confort à 30 %, appareil à 10 %. « Baisser » à 60 % ne
/// passe pas le confort.
#[tokio::test]
async fn le_confort_se_mesure_depuis_le_niveau_reel_6721() {
    let (orch, zone_id) = zone("dlna", 0.80).await;
    SettingsRepo::with_backend(orch.db.clone())
        .set(&crate::audio::volume_limites::cle_confort(zone_id), "0.3")
        .unwrap();
    orch.niveaux_des_appareils.noter(zone_id, 0.10);
    orch.set_volume_avec_garde(zone_id, 0.60, Some(APPAREIL), Hausse::Confirmee)
        .await
        .unwrap();
    let recu = recus(&orch).await;
    assert!(
        recu.iter().all(|v| *v <= 0.30 + 1e-9),
        "#6721 : confort à 30 %, appareil à 10 % : reçu {recu:?}"
    );
}

/// Sans niveau connu (aucun relevé), rien ne change : la mémoire est le repère.
#[tokio::test]
async fn sans_niveau_connu_la_memoire_reste_le_repere_6721() {
    let (orch, zone_id) = zone("dlna", 0.80).await;
    orch.set_volume(zone_id, 0.60, Some(APPAREIL))
        .await
        .unwrap();
    assert_eq!(recus(&orch).await, vec![0.60]);
}
