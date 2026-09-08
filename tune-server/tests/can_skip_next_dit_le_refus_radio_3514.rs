//! `can_skip_next` dit le refus « radio hors file » (#3514).
//!
//! ## Le fait
//!
//! Le correctif de #3342 fait refuser `POST /zones/{id}/next` quand une radio
//! joue hors de sa file : la route rend `{"reason":"radio_no_next"}` et ne
//! touche à rien. C'est le bon comportement — la file résiduelle de Philippe
//! appartenait à une autre écoute.
//!
//! Mais `can_skip_next`, le booléen que les deux documents de zone publient et
//! dont l'écran tire l'état du bouton « suivant », est resté la projection de
//! `PositionPoller::next_position_manual`. Celle-ci ne connaît que la POSITION :
//! sur la file résiduelle — trois pistes Qobuz, position 0 — elle rend `Some(1)`,
//! donc `true`. Pendant toute la radio, le bouton restait franchement actif et
//! chaque appui partait dans le vide, sans que rien ne l'explique.
//!
//! ## Ce que ce fichier cloue
//!
//! 1. les DEUX documents de zone servis au client — `GET /zones` (liste) et
//!    `GET /zones/{id}` (fiche) — disent `can_skip_next: false` pendant une
//!    radio hors file ;
//! 2. l'annonce et le refus RÉEL sont mesurés dans le même test : la même zone,
//!    au même instant, annonce `false` et se voit opposer `radio_no_next` ;
//! 3. LA CONTRE-ÉPREUVE, dans les deux sens et dans ce même fichier — une piste
//!    normale sur la même file annonce `true` ET avance ; une file FAITE de
//!    stations annonce `true` ET avance. Sans elles, un `can_skip_next`
//!    cassé pour tout le monde passerait le premier test.
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

async fn lire(app: &axum::Router, chemin: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET {chemin}");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(json!(null))
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

/// La file de Philippe : trois pistes Qobuz, la zone posée dessus à la position 0.
async fn file_qobuz(state: &AppState, zone_id: i64) {
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
    PlayQueueRepo::with_backend(state.backend.clone())
        .append(zone_id, &items)
        .expect("mise en file");
    state.playback.update_queue_info(zone_id, 0, 3).await;
}

/// La station telle que `play_radio` la déclare à l'orchestrateur : source
/// `radio`, l'URL du flux en `source_id`, et AUCUNE durée.
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

/// Le champ tel que le client le lit dans la LISTE des zones.
fn suivant_dans_la_liste(liste: &Value, zone_id: i64) -> Option<bool> {
    liste
        .as_array()?
        .iter()
        .find(|z| z["id"].as_i64() == Some(zone_id))?
        .get("can_skip_next")?
        .as_bool()
}

/// Le cas de Philippe. La zone joue FIP Jazz, la file résiduelle porte encore
/// l'album Qobuz de l'écoute d'avant. La route REFUSE d'avancer ; les deux
/// documents de zone doivent le dire, sinon l'écran laisse un bouton actif.
#[tokio::test]
async fn les_deux_documents_de_zone_disent_le_refus_pendant_une_radio_hors_file() {
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
    // `play` réinitialise l'information de file : on la repose APRÈS, sinon
    // `can_skip_next` serait faux pour une tout autre raison — et ce test
    // serait vert contre rien.
    state.playback.update_queue_info(zid, 0, 3).await;
    state.playback.update_position(zid, 30_000).await;

    // Repère : sans le refus, la position 0 sur une file de trois a bien une
    // suite. C'est ce qui rendait `can_skip_next` vrai.
    let etat = state.playback.get_state(zid).await;
    assert_eq!(etat.queue_position, 0);
    assert_eq!(etat.queue_length, 3);

    let fiche = lire(&app, &format!("/api/v1/zones/{zid}")).await;
    assert_eq!(
        fiche["can_skip_next"],
        json!(false),
        "GET /zones/{zid} : le serveur refuse le suivant et l'annonce quand \
         même — le bouton reste actif et sans effet (#3514) : {fiche}"
    );

    let liste = lire(&app, "/api/v1/zones").await;
    assert_eq!(
        suivant_dans_la_liste(&liste, zid),
        Some(false),
        "GET /zones : la LISTE est le second document que le client lit, au \
         changement de zone. Corriger la fiche seule laisserait l'écran \
         mentir la moitié du temps : {liste}"
    );

    // Et le refus RÉEL, sur la même zone au même instant : l'annonce et la
    // décision doivent dire la même chose.
    let (status, corps) = poster(&app, &format!("/api/v1/zones/{zid}/next")).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["reason"],
        json!("radio_no_next"),
        "la route refuse bien : c'est CE refus que les deux documents \
         ci-dessus doivent annoncer — {corps}"
    );
    assert_eq!(
        state.playback.get_state(zid).await.queue_position,
        0,
        "la file résiduelle n'a pas bougé"
    );
}

/// CONTRE-ÉPREUVE 1. Même file, même route, même zone — mais une piste locale
/// en cours au lieu d'une radio. L'annonce est `true` ET « suivant » avance.
/// Sans elle, un `can_skip_next` cassé pour tout le monde passerait le test
/// ci-dessus.
#[tokio::test]
async fn une_piste_normale_annonce_son_suivant_et_avance() {
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
    let piste_a = TrackRepo::with_backend(state.backend.clone())
        .get(a)
        .expect("lecture de piste")
        .expect("piste presente");
    state
        .playback
        .play(zid, NowPlaying::from_track(&piste_a))
        .await;
    state.playback.update_queue_info(zid, 0, 2).await;

    let fiche = lire(&app, &format!("/api/v1/zones/{zid}")).await;
    assert_eq!(
        fiche["can_skip_next"],
        json!(true),
        "une piste normale garde son bouton : {fiche}"
    );
    let liste = lire(&app, "/api/v1/zones").await;
    assert_eq!(suivant_dans_la_liste(&liste, zid), Some(true), "{liste}");

    let (status, corps) = poster(&app, &format!("/api/v1/zones/{zid}/next")).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["queue_position"],
        json!(1),
        "et elle avance vraiment : l'annonce ne ment pas dans l'autre sens \
         non plus — {corps}"
    );
}

/// CONTRE-ÉPREUVE 2. Une file FAITE de stations. La règle regarde la ligne de
/// file courante, pas seulement ce qui joue : l'auditeur qui enfile ses
/// stations favorites garde son bouton, et il fonctionne.
#[tokio::test]
async fn une_file_de_stations_annonce_son_suivant_et_avance() {
    let (app, state) = app_et_etat();
    let zid = zone(&state, "Cuisine");
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
    PlayQueueRepo::with_backend(state.backend.clone())
        .append(zid, &items)
        .expect("mise en file");
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

    let fiche = lire(&app, &format!("/api/v1/zones/{zid}")).await;
    assert_eq!(
        fiche["can_skip_next"],
        json!(true),
        "la ligne courante EST une radio : « suivant » y a un sens — {fiche}"
    );
    let liste = lire(&app, "/api/v1/zones").await;
    assert_eq!(suivant_dans_la_liste(&liste, zid), Some(true), "{liste}");

    let (status, corps) = poster(&app, &format!("/api/v1/zones/{zid}/next")).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["queue_position"],
        json!(1),
        "une file de stations avance — {corps}"
    );
}
