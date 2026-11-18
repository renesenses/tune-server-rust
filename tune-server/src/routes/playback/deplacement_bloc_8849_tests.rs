//! Déplacer un album en une transaction, sans changer la piste courante.
use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use tower::ServiceExt;
use tune_core::db::{models::Track, track_repo::TrackRepo, zone_repo::ZoneRepo};

async fn banc() -> (AppState, i64, Vec<i64>) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("File mixte factice", Some("browser"), None)
        .unwrap();
    let tracks = TrackRepo::with_backend(state.backend.clone());
    let mut inputs = Vec::new();
    for n in 0..6 {
        if n % 2 == 0 {
            let id = tracks.create(&Track::new(format!("Locale {n}"))).unwrap();
            inputs.push(QueueInput::Local { track_id: id });
        } else {
            inputs.push(QueueInput::Streaming {
                source: "qobuz".into(),
                source_id: format!("distante-{n}"),
                title: format!("Distante {n}"),
                artist: "Banc".into(),
                album: Some("Album".into()),
                cover_url: None,
                duration_ms: 1000,
                track_number: Some(n),
                disc_number: None,
                album_ref: None,
                artist_ref: None,
            });
        }
    }
    let repo = PlayQueueRepo::with_backend(state.backend.clone());
    repo.append(zone, &inputs).unwrap();
    repo.set_current_pos(zone, 2).unwrap();
    state.playback.update_queue_info(zone, 2, 6).await;
    let ids = repo
        .get_ordered(zone)
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    (state, zone, ids)
}
async fn appel(state: &AppState, zone: i64, corps: Value) -> StatusCode {
    let reponse = super::router()
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/{zone}/queue/move"))
                .header("Content-Type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    reponse.status()
}
fn ids(state: &AppState, zone: i64) -> Vec<i64> {
    PlayQueueRepo::with_backend(state.backend.clone())
        .get_ordered(zone)
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect()
}
#[tokio::test]
async fn le_bloc_mixte_se_deplace_entier_dans_les_deux_sens_8849() {
    for (de, vers, ordre) in [
        (1, 3, vec![0, 4, 5, 1, 2, 3]),
        (3, 0, vec![3, 4, 5, 0, 1, 2]),
    ] {
        let (s, z, avant) = banc().await;
        let mut events = s.event_bus.subscribe();
        assert_eq!(
            appel(
                &s,
                z,
                json!({"from_position":de,"to_position":vers,"count":3,"expected_order":avant})
            )
            .await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            ids(&s, z),
            ordre.iter().map(|i| avant[*i]).collect::<Vec<_>>(),
            "#8849 : le bloc est éclaté en déplaçant seulement sa première piste"
        );
        let mut mouvements = 0;
        while let Ok(e) = events.try_recv() {
            if e.event_type == "playback.queue.moved" {
                mouvements += 1;
            }
        }
        assert_eq!(mouvements, 1);
    }
}
#[tokio::test]
async fn le_curseur_suit_la_meme_piste_deplacee_ou_traversee_8849() {
    for (de, vers, nombre, position) in [(1, 3, 3, 4), (4, 1, 2, 4), (2, 5, 1, 5)] {
        let (s, z, avant) = banc().await;
        assert_eq!(
            appel(
                &s,
                z,
                json!({"from_position":de,"to_position":vers,"count":nombre,"expected_order":avant})
            )
            .await,
            StatusCode::NO_CONTENT
        );
        let etat = s.playback.get_state(z).await;
        assert_eq!(
            etat.queue_position, position,
            "#8849 : le curseur reste sur un rang devenu celui d’une autre piste"
        );
        assert_eq!(ids(&s, z)[etat.queue_position as usize], avant[2]);
        let lignes = PlayQueueRepo::with_backend(s.backend.clone())
            .get_ordered(z)
            .unwrap();
        assert_eq!(lignes.iter().find(|e| e.is_current).unwrap().id, avant[2]);
    }
}
#[tokio::test]
async fn un_ordre_client_perime_est_refuse_sans_ecriture_8849() {
    let (s, z, avant) = banc().await;
    let mut perime = avant.clone();
    perime.swap(0, 1);
    assert_eq!(
        appel(
            &s,
            z,
            json!({"from_position":1,"to_position":3,"count":3,"expected_order":perime})
        )
        .await,
        StatusCode::CONFLICT,
        "#8849 : une vue périmée déplace les mauvaises lignes"
    );
    assert_eq!(ids(&s, z), avant);
}
#[tokio::test]
async fn les_bornes_du_bloc_sont_refusees_sans_ecriture_8849() {
    for (de, vers, nombre) in [(1, 3, 0), (1, 3, 7), (4, 1, 3), (1, 4, 3), (1, 3, i64::MAX)] {
        let (s, z, avant) = banc().await;
        assert_eq!(
            appel(
                &s,
                z,
                json!({"from_position":de,"to_position":vers,"count":nombre})
            )
            .await,
            StatusCode::BAD_REQUEST,
            "#8849 : taille ou destination de bloc invalide acceptée"
        );
        assert_eq!(ids(&s, z), avant);
    }
}
#[tokio::test]
async fn l_ancien_corps_reste_un_deplacement_unitaire_8849() {
    let (s, z, avant) = banc().await;
    assert_eq!(
        appel(&s, z, json!({"from_position":1,"to_position":4})).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        ids(&s, z),
        [0, 2, 3, 4, 1, 5]
            .iter()
            .map(|i| avant[*i])
            .collect::<Vec<_>>()
    );
}
#[tokio::test]
async fn un_echec_d_ecriture_annule_toute_la_transaction_8849() {
    let (s, z, avant) = banc().await;
    s.backend.execute(&format!("CREATE TRIGGER refus_bloc_8849 BEFORE UPDATE OF position ON queue_items WHEN NEW.id = {} BEGIN SELECT RAISE(ABORT, 'banc rollback'); END",avant[3]),&[]).unwrap();
    assert_eq!(
        appel(&s, z, json!({"from_position":1,"to_position":3,"count":3})).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(ids(&s, z), avant);
    assert_eq!(s.playback.get_state(z).await.queue_position, 2);
}
#[tokio::test]
async fn la_file_annonce_le_support_du_bloc_aux_clients_8849() {
    let (s, z, _) = banc().await;
    let rep = super::router()
        .with_state(s)
        .oneshot(
            Request::builder()
                .uri(format!("/{z}/queue"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rep.status(), StatusCode::OK);
    let corps: Value =
        serde_json::from_slice(&to_bytes(rep.into_body(), 1_000_000).await.unwrap()).unwrap();
    assert_eq!(
        corps["block_move_supported"], true,
        "#8849 : le client ne peut pas distinguer un ancien serveur qui ignore count"
    );
}
