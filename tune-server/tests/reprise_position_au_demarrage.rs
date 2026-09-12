//! La position restaurée au démarrage atteint la demande de lecture (#2876).
//!
//! Sandro, fil 1610, sortie DirettaRenderer UPnP, le 30/08/2026 : « le curseur
//! de temps affiche exactement la position où je m'étais arrêté […] lorsque
//! j'appuie sur Play, le morceau reprend depuis le début (0:00) ». Les deux
//! moitiés de sa phrase sont vraies, et c'est ce qui rend le défaut lisible :
//!
//! - le poller persiste `zones.last_position_ms` tout au long de la lecture ;
//! - `restore_playback_positions` la réinjecte au démarrage dans l'état de
//!   zone, d'où `position_ms` sur `/zones` — c'est CE chiffre que le curseur
//!   affiche, il vient bien du serveur ;
//! - et aucun chemin de lecture ne s'en servait : les `PlayRequest` de
//!   « Lecture après arrêt » posaient tous `seek_ms: None`.
//!
//! Le fil de la preuve : `position_de_reprise` ancre la position dans l'état de
//! zone avant de la passer au `PlayRequest`, et cet ancrage émet l'évènement
//! `seek`. Le voir sortir du bus après un vrai `POST /zones/{id}/play` prouve
//! que la route atteint la position restaurée ; ne pas le voir pour une AUTRE
//! piste prouve qu'elle ne déborde pas sur son voisin.
//!
//! La résolution du flux échoue ici — base `:memory:`, aucune piste, aucune
//! sortie. Sans effet sur ce qui est mesuré : l'ancrage précède l'appel à
//! l'orchestrateur, exactement comme `set_session_context` de #1361.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::playback::NowPlaying;
use tune_server::state::AppState;

const PISTE_INTERROMPUE: i64 = 42;
const POSITION_RESTAUREE: i64 = 151_000;

fn app_et_etat() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn post(app: &axum::Router, path: &str, body: Value) -> StatusCode {
    let requete = Request::builder()
        .method("POST")
        .uri(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.clone().oneshot(requete).await.unwrap().status()
}

async fn poster_sans_corps(app: &axum::Router, path: &str) -> StatusCode {
    let requete = Request::builder()
        .method("POST")
        .uri(path)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(requete).await.unwrap().status()
}

async fn zone(app: &axum::Router) -> i64 {
    let requete = Request::builder()
        .method("POST")
        .uri("/api/v1/zones")
        .header("Content-Type", "application/json")
        .body(Body::from(json!({"name": "Salon"}).to_string()))
        .unwrap();
    let resp = app.clone().oneshot(requete).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "création de zone");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    body["id"].as_i64().expect("un id de zone")
}

/// Rejoue ce que fait `restore_playback_positions` au démarrage du serveur.
async fn redemarrage_avec_position(state: &AppState, zone_id: i64) {
    state
        .playback
        .restore_position(
            zone_id,
            POSITION_RESTAUREE,
            NowPlaying {
                track_id: Some(PISTE_INTERROMPUE),
                title: "Piste interrompue".into(),
                duration_ms: 300_000,
                ..Default::default()
            },
        )
        .await;
}

/// La position que la route a demandée, s'il y en a une.
fn position_demandee(
    rx: &mut tokio::sync::broadcast::Receiver<tune_core::playback::PlaybackEvent>,
) -> Option<i64> {
    let mut vue = None;
    while let Ok(evenement) = rx.try_recv() {
        if evenement.event == "seek" {
            vue = evenement.data["position_ms"].as_i64();
        }
    }
    vue
}

/// LE contrat de #2876 : le bouton Lecture d'un client — `{"track_id": N}`,
/// la forme qu'envoient le client web et Flutter quand la zone est à l'arrêt —
/// repart à la position que le démarrage a restaurée.
#[tokio::test]
async fn la_lecture_repart_a_la_position_restauree() {
    let (app, state) = app_et_etat();
    let id = zone(&app).await;
    redemarrage_avec_position(&state, id).await;

    let mut rx = state.playback.subscribe();
    let status = post(
        &app,
        &format!("/api/v1/zones/{id}/play"),
        json!({"track_id": PISTE_INTERROMPUE}),
    )
    .await;
    assert_ne!(status, StatusCode::NOT_FOUND, "la route doit exister");

    assert_eq!(
        position_demandee(&mut rx),
        Some(POSITION_RESTAUREE),
        "le curseur affichait 2:31 et la lecture repartait de 0:00 (#2876)"
    );
}

/// `POST /zones/{id}/resume` — l'autre porte, celle des clients qui distinguent
/// « reprendre » de « lire ». Son commentaire disait « re-play the current
/// track from the start », en contradiction avec celui de
/// `PlaybackManager::stop` : « keep position_ms […] can resume from the same
/// position ».
#[tokio::test]
async fn la_reprise_repart_a_la_position_restauree() {
    let (app, state) = app_et_etat();
    let id = zone(&app).await;
    redemarrage_avec_position(&state, id).await;

    let mut rx = state.playback.subscribe();
    let status = poster_sans_corps(&app, &format!("/api/v1/zones/{id}/resume")).await;
    assert_ne!(status, StatusCode::NOT_FOUND, "la route doit exister");

    assert_eq!(
        position_demandee(&mut rx),
        Some(POSITION_RESTAUREE),
        "la reprise a ignoré la position que le serveur venait de restaurer (#2876)"
    );
}

/// Témoin anti-régression, le risque propre à ce correctif : une AUTRE piste
/// ne doit pas hériter de la position de celle qui a été interrompue. Démarrer
/// un morceau à 2:31 parce qu'un voisin s'y était arrêté serait un défaut pire
/// que celui qu'on répare.
#[tokio::test]
async fn une_autre_piste_ne_herite_pas_de_la_position() {
    let (app, state) = app_et_etat();
    let id = zone(&app).await;
    redemarrage_avec_position(&state, id).await;

    let mut rx = state.playback.subscribe();
    post(
        &app,
        &format!("/api/v1/zones/{id}/play"),
        json!({"track_id": PISTE_INTERROMPUE + 1}),
    )
    .await;

    assert_eq!(
        position_demandee(&mut rx),
        None,
        "une piste qui n'est pas celle restaurée doit commencer à son début"
    );
}

/// Second témoin : un CONTENANT est un nouveau geste d'écoute. Il commence à
/// son début même si sa première piste se trouve être celle que le démarrage a
/// restaurée — c'est la garde `demande_nue`.
#[tokio::test]
async fn un_contenant_commence_a_son_debut() {
    let (app, state) = app_et_etat();
    let id = zone(&app).await;
    redemarrage_avec_position(&state, id).await;

    let mut rx = state.playback.subscribe();
    post(
        &app,
        &format!("/api/v1/zones/{id}/play"),
        json!({"track_ids": [PISTE_INTERROMPUE]}),
    )
    .await;

    assert_eq!(
        position_demandee(&mut rx),
        None,
        "une file explicite est un nouveau geste : elle commence à son début"
    );
}

/// La position n'est offerte qu'UNE fois. Sans cela, tout Stop ultérieur de la
/// session ramènerait le morceau à l'endroit où le serveur avait redémarré.
#[tokio::test]
async fn la_position_restauree_ne_sert_qu_une_fois() {
    let (app, state) = app_et_etat();
    let id = zone(&app).await;
    redemarrage_avec_position(&state, id).await;

    let mut rx = state.playback.subscribe();
    post(
        &app,
        &format!("/api/v1/zones/{id}/play"),
        json!({"track_id": PISTE_INTERROMPUE}),
    )
    .await;
    assert_eq!(position_demandee(&mut rx), Some(POSITION_RESTAUREE));

    // Le marqueur est consommé par le premier flux ; la relecture qui suit
    // recommence au début.
    state
        .playback
        .play(
            id,
            NowPlaying {
                track_id: Some(PISTE_INTERROMPUE),
                duration_ms: 300_000,
                ..Default::default()
            },
        )
        .await;
    let mut rx = state.playback.subscribe();
    post(
        &app,
        &format!("/api/v1/zones/{id}/play"),
        json!({"track_id": PISTE_INTERROMPUE}),
    )
    .await;
    assert_eq!(
        position_demandee(&mut rx),
        None,
        "la position rendue par la base est à usage unique (#2876)"
    );
}
