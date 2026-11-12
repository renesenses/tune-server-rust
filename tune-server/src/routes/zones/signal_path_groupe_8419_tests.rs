//! #8419 — le chemin du signal d'une lecture groupée dit chaque sortie.

use serde_json::{Map, Value, json};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::playback::NowPlaying;

use super::{injecter_les_sorties_du_groupe, sorties_du_groupe};
use crate::state::AppState;

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn piste() -> NowPlaying {
    NowPlaying {
        title: "Groupe".into(),
        format: Some("flac".into()),
        sample_rate: Some(96_000),
        bit_depth: Some(24),
        duration_ms: 240_000,
        ..Default::default()
    }
}

async fn deux_sorties_groupees(state: &AppState) -> (i64, i64) {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let smsl = repo
        .create("SMSL SU-8", Some("dlna"), Some("dev-smsl"))
        .unwrap();
    let tascam = repo
        .create("TASCAM US-366", Some("dlna"), Some("dev-tascam"))
        .unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            "zone_groups",
            &json!([{"id": 4, "zone_ids": [smsl, tascam], "leader_id": smsl}]).to_string(),
        )
        .unwrap();
    for z in [smsl, tascam] {
        state.playback.play(z, piste()).await;
    }
    (smsl, tascam)
}

#[tokio::test]
async fn une_lecture_groupee_montre_chaque_sortie_et_son_etage_final_8419() {
    let state = etat();
    let (smsl, tascam) = deux_sorties_groupees(&state).await;

    let groupe = sorties_du_groupe(&state, smsl)
        .await
        .expect("#8419 : la zone groupée doit décrire son groupe");
    assert_eq!(groupe["group_id"], 4);
    assert_eq!(groupe["leader_id"], smsl);
    let sorties = groupe["outputs"].as_array().unwrap();
    assert_eq!(
        sorties.len(),
        2,
        "#8419 : les DEUX sorties du groupe sont dites : {groupe}"
    );
    assert_eq!(sorties[0]["name"], "SMSL SU-8");
    assert_eq!(sorties[0]["this_zone"], true);
    assert_eq!(sorties[0]["leader"], true);
    assert_eq!(sorties[1]["zone_id"], tascam);
    assert_eq!(sorties[1]["name"], "TASCAM US-366");
    assert_eq!(sorties[1]["this_zone"], false);
    assert_eq!(sorties[1]["state"], "playing");
    assert!(
        sorties[1]["final_stage"].is_object(),
        "#8419 : ce que reçoit la seconde sortie (étage final) : {groupe}"
    );

    // Vu depuis la suiveuse : même groupe, `this_zone` change de main.
    let depuis = sorties_du_groupe(&state, tascam).await.unwrap();
    assert_eq!(depuis["outputs"][1]["this_zone"], true);
}

#[tokio::test]
async fn une_zone_seule_n_a_pas_de_sorties_de_groupe_8419() {
    let state = etat();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let seule = repo
        .create("Salon", Some("dlna"), Some("dev-seule"))
        .unwrap();
    state.playback.play(seule, piste()).await;
    assert!(sorties_du_groupe(&state, seule).await.is_none());

    let mut obj: Map<String, Value> = Map::new();
    obj.insert("signal_path".into(), json!({"steps": []}));
    injecter_les_sorties_du_groupe(&state, seule, &mut obj).await;
    assert!(
        obj["signal_path"].get("group_outputs").is_none(),
        "clé ABSENTE hors groupe : {obj:?}"
    );
}

#[tokio::test]
async fn sans_chemin_du_signal_rien_n_est_pose_8419() {
    let state = etat();
    let (smsl, _) = deux_sorties_groupees(&state).await;
    let mut obj: Map<String, Value> = Map::new();
    obj.insert("signal_path".into(), Value::Null);
    injecter_les_sorties_du_groupe(&state, smsl, &mut obj).await;
    assert!(obj["signal_path"].is_null());

    obj.insert("signal_path".into(), json!({"steps": []}));
    injecter_les_sorties_du_groupe(&state, smsl, &mut obj).await;
    assert_eq!(
        obj["signal_path"]["group_outputs"]["outputs"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
