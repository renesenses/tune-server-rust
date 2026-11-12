//! #8569 (reliquat de #8462, kiran, fil 2762) — une sortie qui annonce
//! `can_set_volume == false` (volume décoratif d'un Diretta UPnP Renderer,
//! #8538) n'a pas d'étape « Volume » dans le chemin du signal, même quand le
//! volume enregistré de la zone est resté à 50 %.
//!
//! Contre-épreuve : neutraliser le filtre de
//! `sans_volume_d_une_sortie_non_reglable` fait tomber les deux premiers
//! tests ; les témoins (renderer ordinaire, capacités inconnues) restent verts.

use super::*;
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::outputs::OutputCapabilities;
use tune_core::outputs::dlna::DlnaOutput;
use tune_core::playback::NowPlaying;

const DIRETTA: &str = "uuid:diretta-renderer-a7366917f9ddf66e";
const ORDINAIRE: &str = "uuid:renderer-ordinaire-8569";

/// Zone DLNA qui joue un FLAC servi tel quel, volume enregistré `volume`.
fn chemin_dlna(device_id: &str, volume: f64, volume_fixe: bool) -> Value {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("Diretta Renderer", Some("dlna"), Some(device_id))
        .unwrap();
    repo.update_volume(id, volume).unwrap();
    if volume_fixe {
        repo.update_fixed_volume(id, true).unwrap();
    }
    let zone = repo.get(id).unwrap().unwrap();
    let mut t = tune_core::db::models::Track::new("Piste".into());
    t.format = Some("flac".into());
    t.sample_rate = Some(44_100);
    t.bit_depth = Some(16);
    let tid = TrackRepo::with_backend(backend.clone()).create(&t).unwrap();
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Piste".into(),
            track_id: Some(tid),
            format: Some("flac".into()),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        }),
        volume: volume / 100.0,
        ..Default::default()
    };
    let fil = StreamInfo {
        format: "flac".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        ..Default::default()
    };
    build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("Diretta Renderer"),
        "none",
        Some(&fil),
    )
    .unwrap()
}

fn a_une_etape_volume(sp: &Value) -> bool {
    sp["steps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["name"] == "Volume")
}

fn capacites(can_set_volume: bool) -> OutputCapabilities {
    OutputCapabilities {
        can_set_volume,
        ..Default::default()
    }
}

#[test]
fn une_sortie_au_volume_non_reglable_n_a_pas_d_etape_volume_8569() {
    let brut = chemin_dlna(DIRETTA, 50.0, false);
    assert!(
        a_une_etape_volume(&brut),
        "prémisse : le constructeur seul émet « Volume du renderer 50% »"
    );
    let verdict_avant = brut["bit_perfect"].clone();
    let sp = sans_volume_d_une_sortie_non_reglable(Some(brut), Some(&capacites(false))).unwrap();
    assert!(
        !a_une_etape_volume(&sp),
        "#8569 : volume décoratif (can_set_volume=false), l'étape Volume reste affichée : {:?}",
        sp["steps"]
    );
    // Le verdict ne bouge pas : l'étape était marquée intacte.
    assert_eq!(sp["bit_perfect"], verdict_avant);
}

#[tokio::test]
async fn le_chemin_lit_la_capacite_de_la_sortie_enregistree_8569() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    {
        let mut sorties = state.outputs.lock().await;
        for udn in [DIRETTA, ORDINAIRE] {
            sorties.register(Box::new(DlnaOutput::new(
                "C19".into(),
                udn.into(),
                "http://127.0.0.1:9".into(),
                "http://127.0.0.1:9/control".into(),
                "http://127.0.0.1:9/control".into(),
                None,
            )));
        }
    }
    let diretta = signal_path_selon_la_sortie(
        &state,
        Some(DIRETTA),
        Some(chemin_dlna(DIRETTA, 50.0, false)),
    )
    .await
    .unwrap();
    assert!(
        !a_une_etape_volume(&diretta),
        "#8569 : zone sur un Diretta UPnP Renderer enregistré, l'étape Volume reste : {:?}",
        diretta["steps"]
    );
    // Témoin : un renderer ordinaire à 50 % garde son étape.
    let ordinaire = signal_path_selon_la_sortie(
        &state,
        Some(ORDINAIRE),
        Some(chemin_dlna(ORDINAIRE, 50.0, false)),
    )
    .await
    .unwrap();
    assert!(a_une_etape_volume(&ordinaire));
    // Témoin : sortie non enregistrée (capacités inconnues), rien ne change.
    let inconnue = signal_path_selon_la_sortie(
        &state,
        Some("uuid:absente"),
        Some(chemin_dlna("uuid:absente", 50.0, false)),
    )
    .await
    .unwrap();
    assert!(a_une_etape_volume(&inconnue));
}

#[test]
fn temoins_volume_reglable_et_capacites_inconnues_gardent_l_etape_8569() {
    let sp = sans_volume_d_une_sortie_non_reglable(
        Some(chemin_dlna(ORDINAIRE, 50.0, false)),
        Some(&capacites(true)),
    )
    .unwrap();
    assert!(a_une_etape_volume(&sp));
    let sp = sans_volume_d_une_sortie_non_reglable(Some(chemin_dlna(ORDINAIRE, 50.0, false)), None)
        .unwrap();
    assert!(a_une_etape_volume(&sp));
    assert!(sans_volume_d_une_sortie_non_reglable(None, Some(&capacites(false))).is_none());
}

/// Cohérence avec l'existant : la zone en volume fixe n'avait déjà pas
/// d'étape, et un volume enregistré à 100 % non plus.
#[test]
fn volume_fixe_et_plein_volume_restent_sans_etape_8569() {
    assert!(!a_une_etape_volume(&chemin_dlna(ORDINAIRE, 50.0, true)));
    assert!(!a_une_etape_volume(&chemin_dlna(ORDINAIRE, 100.0, false)));
}
