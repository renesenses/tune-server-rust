//! #6541 — les limites de volume d'une zone, tenues au point de passage
//! unique (`set_volume`) et comptées SUR L'APPAREIL (sortie factice) : la
//! seule question qui vaille est « qu'a reçu l'enceinte ? ».
use std::sync::Arc;

use tokio::sync::Mutex;

// #6721 — les hausses de ces bancs sont CONFIRMÉES quand la garde contre
// les montées brusques n'est pas l'objet du test (`garde_de_hausse_6721_tests`).
use super::PlaybackOrchestrator;
use crate::audio::volume_limites::{cle_confort, cle_securite};
use crate::db::migrations::run_migrations;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::mock::MockOutput;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

const APPAREIL: &str = "dlna-salon";

/// Une zone DLNA à 20 %, sécurité 70 %, confort 30 % (les chiffres du fil).
async fn zone_limitee(reglages: &[(&str, &str)]) -> (PlaybackOrchestrator, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let repo = ZoneRepo::with_backend(db.clone());
    let zone_id = repo.create("Salon", Some("dlna"), Some(APPAREIL)).unwrap();
    repo.update_volume(zone_id, 20.0).unwrap();
    orch.playback.set_volume(zone_id, 0.2).await;
    let settings = SettingsRepo::with_backend(db);
    settings.set(&cle_securite(zone_id), "0.7").unwrap();
    settings.set(&cle_confort(zone_id), "0.3").unwrap();
    for (cle, valeur) in reglages {
        settings
            .set(&cle.replace("{id}", &zone_id.to_string()), valeur)
            .unwrap();
    }
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Salon").with_type("dlna"),
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

fn volume_en_base(orch: &PlaybackOrchestrator, zone_id: i64) -> f64 {
    ZoneRepo::with_backend(orch.db.clone())
        .get(zone_id)
        .unwrap()
        .unwrap()
        .volume
}

fn proche(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// LE besoin du fil 2335 : le curseur poussé « tout à droite » n'emmène plus
/// l'enceinte à 100 % — il s'arrête au confort (30 %).
#[tokio::test]
async fn le_curseur_tout_a_droite_s_arrete_au_confort_6541() {
    let (orch, zone_id) = zone_limitee(&[]).await;
    orch.set_volume(zone_id, 1.0, Some(APPAREIL)).await.unwrap();
    let recus = recus(&orch).await;
    assert_eq!(recus.len(), 1);
    assert!(
        proche(recus[0], 0.3),
        "confort 30 % : le curseur à 100 % doit partir à 0,3, pas {}",
        recus[0]
    );
    assert!(proche(volume_en_base(&orch, zone_id), 30.0));
}

/// Les boutons montent au-dessus du confort, jusqu'à la sécurité, jamais
/// au-delà.
#[tokio::test]
async fn les_boutons_montent_jusqu_a_la_securite_et_pas_plus_6541() {
    let (orch, zone_id) = zone_limitee(&[]).await;
    orch.set_volume_au_pas_avec_garde(
        zone_id,
        0.5,
        Some(APPAREIL),
        crate::audio::garde_de_hausse::Hausse::Confirmee,
    )
    .await
    .unwrap();
    orch.set_volume_au_pas_avec_garde(
        zone_id,
        0.95,
        Some(APPAREIL),
        crate::audio::garde_de_hausse::Hausse::Confirmee,
    )
    .await
    .unwrap();
    let recus = recus(&orch).await;
    assert!(proche(recus[0], 0.5), "{recus:?}");
    assert!(
        proche(recus[1], 0.7),
        "sécurité 70 % : un pas vers 95 % s'arrête à 0,7 ({recus:?})"
    );
    assert!(proche(volume_en_base(&orch, zone_id), 70.0));
}

/// Un trim de gain positif ne fait pas passer la sécurité à l'appareil : la
/// consigne 70 % × +6 dB partait à 100 %.
#[tokio::test]
async fn un_trim_positif_ne_franchit_pas_la_securite_6541() {
    let (orch, zone_id) = zone_limitee(&[("zone_{id}_gain_trim_db", "6")]).await;
    orch.set_volume_au_pas_avec_garde(
        zone_id,
        0.7,
        Some(APPAREIL),
        crate::audio::garde_de_hausse::Hausse::Confirmee,
    )
    .await
    .unwrap();
    let recus = recus(&orch).await;
    assert!(
        proche(recus[0], 0.7),
        "le trim de +6 dB est borné par la sécurité : {recus:?}"
    );
    // La consigne de l'utilisateur reste ce qu'elle est.
    assert!(proche(volume_en_base(&orch, zone_id), 70.0));
}

/// Décision du 21/10 : sous PURE verrouillé, une limite enregistrée est
/// IGNORÉE — le 100 % part tel qu'avant #6541, sans être abaissé.
#[tokio::test]
async fn pure_verrouille_ignore_la_limite_enregistree_6541() {
    let (orch, zone_id) = zone_limitee(&[(
        "zone_{id}_audiophile",
        r#"{"enabled":true,"lock_volume":true}"#,
    )])
    .await;
    // #6721 — une consigne basse est refusée sous PURE verrouillé ; seule une
    // consigne à 100 % y part.
    orch.set_volume(zone_id, 1.0, Some(APPAREIL)).await.unwrap();
    orch.set_volume_pure_force(zone_id, Some(APPAREIL))
        .await
        .unwrap();
    assert_eq!(
        recus(&orch).await,
        vec![1.0, 1.0],
        "PURE verrouillé : la limite de 70 % n'a pas cours, l'appareil reçoit 100 %"
    );
}

/// L'armement du volume fixe part à 100 % : sur une zone à volume fixe, la
/// limite n'a pas cours (décision du 21/10).
#[tokio::test]
async fn l_armement_du_volume_fixe_ignore_la_limite_6541() {
    let (orch, zone_id) = zone_limitee(&[]).await;
    ZoneRepo::with_backend(orch.db.clone())
        .update_fixed_volume(zone_id, true)
        .unwrap();
    orch.arm_fixed_volume(zone_id, Some(APPAREIL))
        .await
        .unwrap();
    assert_eq!(recus(&orch).await, vec![1.0]);
}

/// Une limite posée AVANT le passage en PURE (non verrouillé) est ignorée tant
/// que la zone y reste, puis reprend effet à la sortie — pour les gestes
/// suivants, et jamais à la hausse.
#[tokio::test]
async fn une_limite_ignoree_en_pure_reprend_a_la_sortie_sans_monter_6541() {
    let (orch, zone_id) = zone_limitee(&[(
        "zone_{id}_audiophile",
        r#"{"enabled":true,"lock_volume":false}"#,
    )])
    .await;
    orch.set_volume_avec_garde(
        zone_id,
        0.9,
        Some(APPAREIL),
        crate::audio::garde_de_hausse::Hausse::Confirmee,
    )
    .await
    .unwrap();
    assert_eq!(
        recus(&orch).await,
        vec![0.9],
        "en PURE, ni le confort (30 %) ni la sécurité (70 %) ne retiennent le curseur"
    );
    SettingsRepo::with_backend(orch.db.clone())
        .set(
            &format!("zone_{zone_id}_audiophile"),
            r#"{"enabled":false}"#,
        )
        .unwrap();
    // Sortie de PURE : rien n'est envoyé de lui-même.
    assert_eq!(recus(&orch).await, vec![0.9]);
    // Le geste suivant est borné : 100 % demandé, 70 % reçus (une baisse).
    orch.set_volume(zone_id, 1.0, Some(APPAREIL)).await.unwrap();
    orch.set_volume_au_pas(zone_id, 0.95, Some(APPAREIL))
        .await
        .unwrap();
    let recus = recus(&orch).await;
    assert!(proche(recus[1], 0.7), "{recus:?}");
    assert!(proche(recus[2], 0.7), "{recus:?}");
}

/// Au-dessus du confort (montée aux boutons), le curseur peut redescendre.
#[tokio::test]
async fn au_dessus_du_confort_le_curseur_redescend_6541() {
    let (orch, zone_id) = zone_limitee(&[]).await;
    orch.set_volume_au_pas_avec_garde(
        zone_id,
        0.5,
        Some(APPAREIL),
        crate::audio::garde_de_hausse::Hausse::Confirmee,
    )
    .await
    .unwrap();
    orch.set_volume(zone_id, 0.4, Some(APPAREIL)).await.unwrap();
    orch.set_volume(zone_id, 0.6, Some(APPAREIL)).await.unwrap();
    let recus = recus(&orch).await;
    assert!(proche(recus[1], 0.4), "{recus:?}");
    assert!(
        proche(recus[2], 0.4),
        "le curseur ne remonte pas au-dessus de max(confort, actuel) : {recus:?}"
    );
}

/// TÉMOIN : sans limite, le même banc envoie bien 100 % — la preuve que les
/// tests ci-dessus ne sont pas verts sur un banc qui bornerait tout seul.
#[tokio::test]
async fn temoin_sans_limite_le_curseur_part_a_100() {
    let (orch, zone_id) = zone_limitee(&[]).await;
    let settings = SettingsRepo::with_backend(orch.db.clone());
    settings.delete(&cle_securite(zone_id)).unwrap();
    settings.delete(&cle_confort(zone_id)).unwrap();
    orch.set_volume_avec_garde(
        zone_id,
        1.0,
        Some(APPAREIL),
        crate::audio::garde_de_hausse::Hausse::Confirmee,
    )
    .await
    .unwrap();
    assert_eq!(recus(&orch).await, vec![1.0]);
}
