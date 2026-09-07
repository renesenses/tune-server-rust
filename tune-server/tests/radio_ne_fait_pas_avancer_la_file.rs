//! Une radio ne fait pas avancer la file (#3342).
//!
//! Philippe, forum du 04/09/2026, v0.9.134 : « lorsque j'écoute une station
//! radio, au bout d'une dizaine de secondes la lecture passe sur le dernier
//! album Qobuz écouté sans action de ma part. Bug reproduit avec plusieurs
//! stations radios. »
//!
//! Ses journaux joints donnent la chronologie, six fois de suite dans le même
//! quart d'heure, sur cinq stations différentes :
//!
//! ```text
//! 17:33:32.096  orchestrator_play zone_id=10 title=FIP Jazz source=radio
//! 17:33:42.175  radio_stream_superseded ... connected_secs=9
//! 17:33:42.443  radio_stream_client_disconnect ... remaining_consumers=0
//! 17:33:46.154  api_next_requested zone_id=10
//! 17:33:46.158  prefetch_consumed source=qobuz source_id=410609160
//! 17:33:46.331  orchestrator_play zone_id=10 title=Simplifier source=qobuz
//! ```
//!
//! `play_radio` ne touche pas la file : la zone garde la `queue_position` et
//! la `queue_length` de l'écoute précédente. Celle de Philippe portait encore
//! huit pistes Qobuz, position 0 — et `POST /zones/{id}/next` la ressuscitait
//! à la position 1. Le sondeur avait déjà cette garde depuis #2493 ; la route
//! HTTP ne l'avait jamais eue.
//!
//! Ce que ce fichier cloue :
//!
//! 1. une radio qui joue depuis trente secondes ne fait pas avancer la file,
//!    quel que soit le nombre d'appels à « suivant » ;
//! 2. la CONTRE-ÉPREUVE est dans le même fichier : la même file, la même
//!    route, une piste locale en cours — « suivant » avance normalement. Ce
//!    n'est ni la file ni la route qui bloquent, c'est la radio ;
//! 3. une file FAITE de stations garde son « suivant » : la garde regarde la
//!    ligne de file courante, pas seulement ce qui joue.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::models::Track;
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::playback::NowPlaying;
use tune_server::state::AppState;

fn app_et_etat() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn poster(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::post(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

/// Une zone posée EN BASE, avec une sortie : `reject_if_zone_has_no_output_device`
/// refuse « suivant » sur une zone orpheline, et ce n'est pas ce qui est éprouvé ici.
fn zone(state: &AppState, nom: &str) -> i64 {
    ZoneRepo::with_backend(state.backend.clone())
        .create(nom, Some("mock"), Some("sortie-essai"))
        .expect("creation de zone")
}

fn piste(state: &AppState, titre: &str) -> i64 {
    let mut t = Track::new(titre.into());
    t.file_path = Some(format!("/musique/{titre}.flac"));
    t.format = Some("flac".into());
    t.duration_ms = 240_000;
    TrackRepo::with_backend(state.backend.clone())
        .create(&t)
        .expect("insertion de piste")
}

/// La file de Philippe : des pistes en streaming, dans l'espace unifié, et la
/// zone posée dessus à la position 0.
async fn file_qobuz(state: &AppState, zone_id: i64) {
    let repo = PlayQueueRepo::with_backend(state.backend.clone());
    let items: Vec<QueueInput> = [
        ("410609158", "L'absence"),
        ("410609159", "Black"),
        ("410609160", "Simplifier"),
    ]
    .iter()
    .map(|(id, titre)| QueueInput::Streaming {
        source: "qobuz".into(),
        source_id: (*id).into(),
        title: (*titre).to_string(),
        artist: "Aldo Romano".into(),
        album: None,
        duration_ms: 197_000,
        cover_url: None,
        track_number: None,
        disc_number: None,
    })
    .collect();
    repo.append(zone_id, &items).expect("mise en file");
    state.playback.update_queue_info(zone_id, 0, 3).await;
}

/// La station telle que `play_radio` la déclare à l'orchestrateur : source
/// `radio`, l'URL du flux en `source_id`, et AUCUNE durée — une radio n'a pas
/// de fin.
fn station(nom: &str, url: &str) -> NowPlaying {
    NowPlaying {
        title: nom.into(),
        artist_name: Some("Live Radio".into()),
        album_title: Some(nom.into()),
        duration_ms: 0,
        source: "radio".into(),
        source_id: Some(url.into()),
        ..Default::default()
    }
}

/// Le cas de Philippe. La zone joue FIP Jazz depuis trente secondes, la file
/// résiduelle porte encore l'album Qobuz de l'écoute d'avant. « Suivant » —
/// que ce soit son doigt ou le réveil de son navigateur, le serveur ne peut
/// pas les distinguer — ne doit RIEN faire sortir de cette file.
#[tokio::test]
async fn une_radio_qui_joue_depuis_trente_secondes_ne_fait_pas_avancer_la_file() {
    let (app, state) = app_et_etat();
    let zid = zone(&state, "Salon");
    file_qobuz(&state, zid).await;

    state
        .playback
        .play(
            zid,
            station(
                "FIP Jazz",
                "https://icecast.radiofrance.fr/fipjazz-hifi.aac",
            ),
        )
        .await;
    state.playback.update_queue_info(zid, 0, 3).await;
    // La radio joue depuis trente secondes : bien au-delà de la dizaine de
    // secondes au bout de laquelle Philippe voyait basculer.
    state.playback.update_position(zid, 30_000).await;

    // Trois appels d'affilée : le journal de Philippe en montre six en un
    // quart d'heure, chacun avançant d'un cran dans son album.
    for essai in 1..=3 {
        let (status, corps) = poster(&app, &format!("/api/v1/zones/{zid}/next")).await;
        assert_eq!(status, StatusCode::OK, "essai {essai} : {corps}");
        assert_eq!(
            corps["reason"],
            json!("radio_no_next"),
            "essai {essai} : la route doit dire pourquoi elle n'avance pas — {corps}"
        );
        assert_eq!(
            corps["queue_position"],
            json!(0),
            "essai {essai} : la file residuelle ne bouge pas — {corps}"
        );
    }

    let etat = state.playback.get_state(zid).await;
    assert_eq!(etat.queue_position, 0, "la position de file n'a pas bouge");
    assert_eq!(
        etat.now_playing.as_ref().map(|np| np.source.as_str()),
        Some("radio"),
        "la zone joue toujours la radio, pas Qobuz"
    );
    assert_eq!(
        etat.now_playing.as_ref().map(|np| np.title.as_str()),
        Some("FIP Jazz"),
    );
}

/// LA CONTRE-ÉPREUVE. Même file, même route, même zone — mais une piste
/// locale en cours au lieu d'une radio. « Suivant » avance, comme avant.
/// Sans elle, le test ci-dessus passerait aussi avec un `next` cassé pour
/// tout le monde.
#[tokio::test]
async fn une_piste_normale_garde_son_suivant() {
    let (app, state) = app_et_etat();
    let zid = zone(&state, "Bureau");
    let a = piste(&state, "A");
    PlayQueueRepo::with_backend(state.backend.clone())
        .append(
            zid,
            &[
                QueueInput::Local { track_id: a },
                QueueInput::Local {
                    track_id: piste(&state, "B"),
                },
            ],
        )
        .expect("mise en file");
    state.playback.update_queue_info(zid, 0, 2).await;

    let piste_a = TrackRepo::with_backend(state.backend.clone())
        .get(a)
        .expect("lecture de piste")
        .expect("piste presente");
    state
        .playback
        .play(zid, NowPlaying::from_track(&piste_a))
        .await;
    state.playback.update_queue_info(zid, 0, 2).await;

    let (status, corps) = poster(&app, &format!("/api/v1/zones/{zid}/next")).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["queue_position"],
        json!(1),
        "une piste normale avance toujours d'un cran — {corps}"
    );
    assert_eq!(
        corps["reason"],
        json!(null),
        "aucun refus ne doit etre oppose a une piste normale — {corps}"
    );
}

/// Une file FAITE de stations garde son « suivant » : la garde ne regarde pas
/// « est-ce une radio qui joue », elle regarde si la file sur laquelle la zone
/// est posée est la sienne. Sans cette distinction, un utilisateur qui enfile
/// ses stations favorites perdrait le bouton.
#[tokio::test]
async fn une_file_de_stations_garde_son_suivant() {
    let (app, state) = app_et_etat();
    let zid = zone(&state, "Cuisine");
    let repo = PlayQueueRepo::with_backend(state.backend.clone());
    let items: Vec<QueueInput> = [
        (
            "https://icecast.radiofrance.fr/fipjazz-hifi.aac",
            "FIP Jazz",
        ),
        (
            "https://icecast.radiofrance.fr/fipworld-hifi.aac",
            "FIP Monde",
        ),
    ]
    .iter()
    .map(|(url, nom)| QueueInput::Streaming {
        source: "radio".into(),
        source_id: (*url).into(),
        title: (*nom).to_string(),
        artist: "Live Radio".into(),
        album: None,
        duration_ms: 0,
        cover_url: None,
        track_number: None,
        disc_number: None,
    })
    .collect();
    repo.append(zid, &items).expect("mise en file");
    state.playback.update_queue_info(zid, 0, 2).await;

    state
        .playback
        .play(
            zid,
            station(
                "FIP Jazz",
                "https://icecast.radiofrance.fr/fipjazz-hifi.aac",
            ),
        )
        .await;
    state.playback.update_queue_info(zid, 0, 2).await;

    let (status, corps) = poster(&app, &format!("/api/v1/zones/{zid}/next")).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["queue_position"],
        json!(1),
        "une file de stations avance : la ligne courante EST une radio — {corps}"
    );
}
