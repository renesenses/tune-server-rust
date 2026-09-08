//! L'album de ce qui joue, en UNE réponse (#1361).
//!
//! **Cyrille Moutia** tient le registre depuis le 30/06/2026 et a relancé le
//! 09/08 : le titre d'album de « Lecture en cours » doit ramener à l'album, et
//! il n'existe toujours pas de « Retour à l'album en cours ».
//!
//! Le serveur avait déjà les morceaux — le contexte de session (#2441,
//! `91184173`), l'identifiant d'artiste des pistes de service (`431e1b44`), et
//! `GET /streaming/{service}/tracks/{track_id}` — mais leur assemblage était
//! laissé à chaque client, avec à chaque fois la question « chez qui ouvrir
//! cet identifiant ? ». Ce témoin attaque la route publique qui répond à cette
//! question, pas la fonction interne : ce qui doit être prouvé est le CÂBLAGE.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn get_json(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Le cas du ticket : un album Qobuz lancé depuis l'écran Qobuz.
#[tokio::test]
async fn le_geste_album_qobuz_donne_la_fiche_qobuz() {
    let (app, state) = app_et_etat();
    state
        .playback
        .set_session_context(
            1,
            Some("album".into()),
            Some("0060254735822".into()),
            Some("qobuz".into()),
        )
        .await;

    let (status, body) = get_json(&app, "/api/v1/zones/1/album-en-cours").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "streaming");
    assert_eq!(body["service"], "qobuz");
    assert_eq!(body["album_id"], "0060254735822");
    assert_eq!(
        body["path"], "/api/v1/streaming/qobuz/albums/0060254735822",
        "le client n'a qu'à ouvrir ce chemin — ni recherche par titre, ni \
         supposition de service"
    );
    assert_eq!(body["origin"], "session_context");
}

/// La même route sous son autre orthographe : un client ne doit pas avoir à
/// deviner de quel côté de la francisation du dépôt il est tombé.
#[tokio::test]
async fn l_alias_anglais_repond_la_meme_chose() {
    let (app, state) = app_et_etat();
    state
        .playback
        .set_session_context(
            1,
            Some("album".into()),
            Some("0060254735822".into()),
            Some("qobuz".into()),
        )
        .await;

    let (s1, b1) = get_json(&app, "/api/v1/zones/1/album-en-cours").await;
    let (s2, b2) = get_json(&app, "/api/v1/zones/1/current-album").await;
    assert_eq!(s1, s2);
    assert_eq!(b1, b2);
}

/// Un album de la BIBLIOTHÈQUE sort dans l'autre espace de noms, et le chemin
/// le dit.
#[tokio::test]
async fn un_album_local_sort_avec_le_chemin_de_bibliotheque() {
    let (app, state) = app_et_etat();
    state
        .playback
        .set_session_context(
            2,
            Some("album".into()),
            Some("42".into()),
            Some("local".into()),
        )
        .await;

    let (status, body) = get_json(&app, "/api/v1/zones/2/album-en-cours").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "library");
    assert_eq!(body["service"], "local");
    assert_eq!(body["path"], "/api/v1/library/albums/42");
    assert_eq!(body["origin"], "session_context");
}

/// Sans geste d'album — une piste lancée seule, puis la file qui avance — la
/// ligne de bibliothèque de la piste en cours répond.
#[tokio::test]
async fn sans_geste_d_album_la_piste_en_cours_repond() {
    let (app, state) = app_et_etat();
    state
        .playback
        .play(
            3,
            tune_core::playback::NowPlaying {
                title: "Melody".into(),
                source: "local".into(),
                album_id: Some(7),
                artist_id: Some(3),
                ..Default::default()
            },
        )
        .await;

    let (status, body) = get_json(&app, "/api/v1/zones/3/album-en-cours").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "library");
    assert_eq!(body["album_id"], "7");
    assert_eq!(body["artist_id"], "3");
    assert_eq!(body["path"], "/api/v1/library/albums/7");
    assert_eq!(body["origin"], "current_track");
}

/// Contre-épreuve n° 1 — une zone qui ne joue rien ne fabrique pas d'album.
/// C'est ce qui permet au client de MASQUER le raccourci plutôt que de
/// l'afficher mort.
#[tokio::test]
async fn une_zone_muette_ne_fabrique_pas_d_album() {
    let (app, _state) = app_et_etat();
    let (status, body) = get_json(&app, "/api/v1/zones/4/album-en-cours").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["album_id"], Value::Null);
    assert_eq!(body["reason"], "aucune_lecture");
}

/// Contre-épreuve n° 2 — un geste de PISTE n'est pas un geste d'album : la
/// première branche ne doit pas confondre l'identifiant d'une piste avec celui
/// d'un album, sans quoi le raccourci ouvrirait une fiche inexistante.
#[tokio::test]
async fn un_geste_de_piste_n_est_pas_un_album() {
    let (app, state) = app_et_etat();
    state
        .playback
        .set_session_context(
            5,
            Some("track".into()),
            Some("143276534".into()),
            Some("qobuz".into()),
        )
        .await;

    let (status, body) = get_json(&app, "/api/v1/zones/5/album-en-cours").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_ne!(body["album_id"], Value::String("143276534".into()));
}

/// Contre-épreuve n° 3 — une radio n'a pas de fiche d'album : la route le dit
/// au lieu d'aller déranger un service pour rien.
#[tokio::test]
async fn une_radio_ne_donne_pas_de_fiche_d_album() {
    let (app, state) = app_et_etat();
    state
        .playback
        .play(
            6,
            tune_core::playback::NowPlaying {
                title: "FIP".into(),
                source: "radio".into(),
                source_id: Some("fip-hifi".into()),
                ..Default::default()
            },
        )
        .await;

    let (status, body) = get_json(&app, "/api/v1/zones/6/album-en-cours").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["reason"], "source_sans_fiche_album");
}
