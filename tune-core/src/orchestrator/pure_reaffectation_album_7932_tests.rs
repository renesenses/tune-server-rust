//! #7932 (fil 2386, rc9) — l'icône PURE se vide après 1 à 2 s en WASAPI
//! exclusif (ampli AV) et en ASIO (DAC), pas en WASAPI partagé.
//!
//! Seules les sorties exclusives publient leur verdict DSP réel : c'est lui
//! que le client lit, au premier bloc poussé. Ce verdict suit les réglages
//! ENREGISTRÉS (`pure_contourne_un_reglage`, #6522) et compte, depuis #6780,
//! la règle de réaffectation des canaux de la ZONE. La règle de l'ALBUM joué,
//! que PURE coupe tout autant (`regle_effective_with` rend `None` en PURE),
//! n'était pas lue : une zone sans règle propre qui joue un album réaffecté
//! annonçait « rien à contourner », et le bouclier se vidait.
//!
//! Sorties factices, aucun son.

use super::PlaybackOrchestrator;
use crate::db::backend::ToSqlValue;
use crate::db::settings_repo::SettingsRepo;
use crate::outputs::traits::{OutputDspState, OutputTarget};
use crate::playback::NowPlaying;
use std::sync::Arc;

const SORTIE: &str = "local:Ampli AV exclusif";

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    )
}

/// Une règle 2 → 2 qui échange gauche et droite : armée, pas l'identité.
const REGLE_ECHANGE: &str = r#"{"enabled":true,"inputs":2,"outputs":2,"gains_db":[[null,0.0],[0.0,null]],"normalize":false,"preset":null}"#;

/// Deux albums, une piste stéréo sur l'album 1, greffon de réaffectation
/// installé et activé, AUCUNE règle de zone.
fn bibliotheque(orch: &PlaybackOrchestrator) {
    for sql in [
        "INSERT INTO artists (id, name) VALUES (1, 'Artiste')",
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album réaffecté', 1)",
        "INSERT INTO albums (id, title, artist_id) VALUES (2, 'Autre album', 1)",
    ] {
        orch.db.execute(sql, &[]).unwrap();
    }
    let chemin = "/nulle/part/piste.flac".to_string();
    orch.db
        .execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
             duration_ms, sample_rate, bit_depth, channels) \
             VALUES (1, 'Piste', 1, 1, ?, 'flac', 60000, 44100, 16, 2)",
            &[&chemin as &dyn ToSqlValue],
        )
        .unwrap();
    let s = SettingsRepo::with_backend(orch.db.clone());
    s.set("plugin_channel-remap_installed", "true").unwrap();
    s.set("plugin_channel-remap_enabled", "true").unwrap();
}

/// Une zone locale qui joue la piste 1 (format déclaré), PURE éteint au
/// départ.
async fn zone_locale_qui_joue(orch: &PlaybackOrchestrator) -> i64 {
    let zone_id = crate::db::zone_repo::ZoneRepo::with_backend(orch.db.clone())
        .create("Ampli AV", Some("local"), Some(SORTIE))
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(crate::outputs::local::LocalOutput::new(
            "Ampli AV exclusif".to_string(),
        )));
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                title: "Piste".into(),
                track_id: Some(1),
                source: "local".into(),
                duration_ms: 60_000,
                ..Default::default()
            },
        )
        .await;
    let arc = orch.outputs.lock().await.get(SORTIE).unwrap();
    let sortie = arc.lock().await;
    let local = sortie
        .as_any()
        .downcast_ref::<crate::outputs::local::LocalOutput>()
        .unwrap();
    local.declare_current_format_for_test(44_100, 2);
    local.set_pure_bypass(false);
    zone_id
}

/// La vraie bascule PURE de l'orchestrateur, puis le verdict que le sondeur
/// lit au bloc suivant.
async fn verdict_apres_la_bascule_pure(
    orch: &PlaybackOrchestrator,
    zone_id: i64,
) -> OutputDspState {
    SettingsRepo::with_backend(orch.db.clone())
        .set(&format!("zone_{zone_id}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    assert!(orch.refresh_zone_pure_dsp(zone_id).await);
    let arc = orch.outputs.lock().await.get(SORTIE).unwrap();
    let sortie = arc.lock().await;
    let local = sortie
        .as_any()
        .downcast_ref::<crate::outputs::local::LocalOutput>()
        .unwrap();
    local.publier_le_contrat_pour_test();
    local.signal_path_status().unwrap().dsp
}

/// LE témoin : aucune règle de zone, une règle d'album armée pour l'album de
/// la piste en cours — PURE la coupe, le verdict doit le dire.
#[tokio::test]
async fn pure_contourne_la_reaffectation_de_l_album_joue_7932() {
    let orch = orchestrateur();
    bibliotheque(&orch);
    let zone_id = zone_locale_qui_joue(&orch).await;
    SettingsRepo::with_backend(orch.db.clone())
        .set(
            &crate::audio::reaffectation_canaux::cle_d_album(1),
            REGLE_ECHANGE,
        )
        .unwrap();

    assert_eq!(
        verdict_apres_la_bascule_pure(&orch, zone_id).await,
        OutputDspState::BypassedPure,
        "#7932 : la réaffectation des canaux armée sur l'ALBUM joué est coupée \
         par PURE — le bouclier doit le dire (« DSP contourné par PURE »), \
         pas « rien à contourner »"
    );
}

/// Les cas où PURE n'a vraiment rien à contourner restent dits tels (#4176) :
/// règle d'un AUTRE album, règle d'album désarmée, règle identité, règle
/// d'album pour un autre nombre de canaux que la piste.
#[tokio::test]
async fn une_regle_d_album_qui_ne_s_applique_pas_reste_rien_a_contourner_7932() {
    let identite = r#"{"enabled":true,"inputs":2,"outputs":2,"gains_db":[[0.0,null],[null,0.0]],"normalize":false,"preset":null}"#;
    let desarmee = REGLE_ECHANGE.replace(r#""enabled":true"#, r#""enabled":false"#);
    let six_canaux = r#"{"enabled":true,"inputs":6,"outputs":2,"gains_db":[[0.0,null,-3.0,null,0.0,null],[null,0.0,-3.0,null,null,0.0]],"normalize":false,"preset":null}"#;
    for (album, regle, quoi) in [
        (2, REGLE_ECHANGE, "règle d'un autre album"),
        (1, desarmee.as_str(), "règle d'album désarmée"),
        (1, identite, "matrice identité"),
        (1, six_canaux, "règle 6 voies sur une piste stéréo"),
    ] {
        let orch = orchestrateur();
        bibliotheque(&orch);
        let zone_id = zone_locale_qui_joue(&orch).await;
        SettingsRepo::with_backend(orch.db.clone())
            .set(
                &crate::audio::reaffectation_canaux::cle_d_album(album),
                regle,
            )
            .unwrap();
        assert_eq!(
            verdict_apres_la_bascule_pure(&orch, zone_id).await,
            OutputDspState::PureSansObjet,
            "{quoi} : PURE n'a rien à contourner, il doit le dire"
        );
    }
}
