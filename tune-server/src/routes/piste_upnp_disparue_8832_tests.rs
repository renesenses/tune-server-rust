//! #8832 — une vue ouverte garde un id que la synchronisation vient de retirer.
use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use tower::ServiceExt;
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::{models::Track, track_repo::TrackRepo, zone_repo::ZoneRepo};

fn banc() -> (AppState, i64, i64, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Banc muet", Some("browser"), None)
        .unwrap();
    let repo = TrackRepo::with_backend(state.backend.clone());
    let mut distante = Track::new("DSF du NAS".into());
    distante.source = "upnp".into();
    distante.source_id = Some("uuid:banc-8832|piste".into());
    distante.format = Some("dsf".into());
    distante.sample_rate = Some(2_822_400);
    distante.bit_depth = Some(1);
    let id = repo.create(&distante).unwrap();
    let gardee = repo.create(&Track::new("File à garder".into())).unwrap();
    PlayQueueRepo::with_backend(state.backend.clone())
        .append(zone, &[QueueInput::Local { track_id: gardee }])
        .unwrap();
    let source = Source {
        key: "banc-8832".into(),
        udn: "uuid:banc-8832".into(),
        container: "0".into(),
        name: "Tune OS synthétique".into(),
        enabled: true,
        status: "confirmation".into(),
        last_attempt: now_seconds(),
        last_success: Some(now_seconds()),
        report: json!({"complet": true}),
        generation: "nouvelle".into(),
        pending: vec![id],
    };
    save(state.backend.as_ref(), &source).unwrap();
    state.backend.execute("INSERT INTO upnp_library_members (source_key, track_id, generation) VALUES ('banc-8832', ?, 'ancienne')", &[&id]).unwrap();
    (state, zone, id, gardee)
}

async fn confirmer(state: &AppState, id: i64) {
    let resultat = act(
        State(state.clone()),
        Json(Action {
            key: "banc-8832".into(),
            action: "confirm".into(),
            generation: Some("nouvelle".into()),
            count: Some(1),
        }),
    )
    .await
    .unwrap();
    assert_eq!(resultat.0["report"]["supprimees"], 1);
    assert!(
        TrackRepo::with_backend(state.backend.clone())
            .get(id)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn le_retrait_upnp_previent_les_vues_de_bibliotheque_8832() {
    let (state, _, id, _) = banc();
    let mut evenements = state.event_bus.subscribe();
    confirmer(&state, id).await;
    let mut types = Vec::new();
    while let Ok(e) = evenements.try_recv() {
        types.push(e.event_type);
    }
    assert!(
        types.iter().any(|t| t == "library.updated"),
        "#8832 : la piste a disparu mais aucune invalidation de bibliothèque n'atteint les vues ouvertes : {types:?}"
    );
}

async fn verifier_refus_sans_perdre_la_file(cas: &str) {
    let (state, zone, id, gardee) = banc();
    confirmer(&state, id).await;
    let demande = match cas {
        "piste" => json!({"track_id": id}),
        "liste" => json!({"track_ids": [id]}),
        "position" => json!({"track_ids": [gardee, id], "start_index": 1}),
        "album" => {
            let album = tune_core::db::album_repo::AlbumRepo::with_backend(state.backend.clone())
                .create(&tune_core::db::models::Album::new("Album conservé".into()))
                .unwrap();
            state
                .backend
                .execute(
                    "UPDATE tracks SET album_id = ? WHERE id = ?",
                    &[&album, &gardee],
                )
                .unwrap();
            json!({"album_id": album, "track_id": id})
        }
        _ => unreachable!(),
    };
    let reponse = crate::routes::playback::router()
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/{zone}/play"))
                .header("Content-Type", "application/json")
                .body(Body::from(demande.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = reponse.status();
    let corps: Value =
        serde_json::from_slice(&to_bytes(reponse.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    let file = PlayQueueRepo::with_backend(state.backend.clone())
        .get_ordered(zone)
        .unwrap()
        .into_iter()
        .filter_map(|e| e.track_id)
        .collect::<Vec<_>>();
    assert_eq!(
        (statut, file),
        (StatusCode::NOT_FOUND, vec![gardee]),
        "#8832 : une vue périmée donne une panne générique ou efface la file valide : {corps}"
    );
    assert_eq!(corps["code"], "track_missing");
    assert_eq!(corps["message_key"], "api.playback.trackMissing");
}

#[tokio::test]
async fn une_piste_retiree_est_signalee_sans_effacer_la_file_8832() {
    verifier_refus_sans_perdre_la_file("piste").await;
}
#[tokio::test]
async fn une_liste_perimee_est_signalee_sans_effacer_la_file_8832() {
    verifier_refus_sans_perdre_la_file("liste").await;
}
#[tokio::test]
async fn une_position_retiree_est_signalee_sans_effacer_la_file_8832() {
    verifier_refus_sans_perdre_la_file("position").await;
}

#[tokio::test]
async fn le_retrait_automatique_previent_une_seule_fois_apres_commit_8832() {
    let (state, _, id, _) = banc();
    let source = sources(state.backend.as_ref()).unwrap().remove(0);
    let mut evenements = state.event_bus.subscribe();
    assert_eq!(
        retirer_et_prevenir(&state, &source, false).await.unwrap(),
        1
    );
    let evenement = evenements.try_recv().unwrap();
    assert_eq!(evenement.event_type, "library.updated");
    assert_eq!(evenement.data["removed"], 1);
    assert!(
        TrackRepo::with_backend(state.backend.clone())
            .get(id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        retirer_et_prevenir(&state, &source, false).await.unwrap(),
        0
    );
    assert!(
        evenements.try_recv().is_err(),
        "un retrait vide ne doit pas invalider la bibliothèque"
    );
}

#[tokio::test]
async fn un_retrait_annule_ne_previent_pas_les_vues_8832() {
    let (state, _, id, _) = banc();
    state
        .backend
        .execute(
            "INSERT INTO favorites (profile_id,item_type,item_id) VALUES (1,'track',?)",
            &[&id.to_string()],
        )
        .unwrap();
    let source = sources(state.backend.as_ref()).unwrap().remove(0);
    let mut evenements = state.event_bus.subscribe();
    assert!(retirer_et_prevenir(&state, &source, false).await.is_err());
    assert!(
        TrackRepo::with_backend(state.backend.clone())
            .get(id)
            .unwrap()
            .is_some()
    );
    assert!(
        evenements.try_recv().is_err(),
        "pas d'événement pour une transaction annulée"
    );
}

#[tokio::test]
async fn une_piste_retiree_ne_se_remplace_pas_par_la_premiere_de_l_album_8832() {
    verifier_refus_sans_perdre_la_file("album").await;
}
