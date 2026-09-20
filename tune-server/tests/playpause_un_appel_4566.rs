//! #4566 — `POST /zones/{id}/playpause` : la bascule lecture/pause en UN appel.
//!
//! Steve Taylor (fil forum 1854, 19/09/2026) pilote Tune avec une télécommande
//! infrarouge — récepteur FLIRC, `triggerhappy`, Ubuntu 24.04. Son câblage n'a
//! qu'une touche pour les deux gestes :
//!
//! ```text
//! KEY_PLAYPAUSE 1 /usr/local/bin/toggle_zone_state.sh 12
//! ```
//!
//! et son script doit donc faire un `GET /zones/{id}`, lire `.state` avec `jq`,
//! puis poster `pause` ou `resume`. `next` et `previous` sont idempotents, un
//! bouton un appel ; `shuffle` avait déjà sa bascule. La lecture, non.
//!
//! Ce que ce fichier cloue :
//!
//! 1. zone en lecture → la bascule MET EN PAUSE ;
//! 2. zone en pause → la bascule REPREND ;
//! 3. ⭐ zone à l'ARRÊT et VIDE → la bascule **refuse en 409 sans rien
//!    écrire**. C'est le cas dangereux du bouton physique, qu'on presse sans
//!    regarder : `resume` seul y aurait annoncé la zone « en lecture » alors
//!    que rien ne joue ;
//! 4. zone à l'arrêt mais qui GARDE une piste en mémoire → elle repart, elle
//!    n'est pas refusée (contre-épreuve du point 3 : ce n'est pas « arrêt =
//!    refus », c'est « rien à jouer = refus ») ;
//! 5. une RADIO en cours se met en pause comme n'importe quelle piste, et la
//!    file ne bouge pas — la bascule délègue, elle n'invente rien.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré en cible `[[test]]` dans `tune-server/Cargo.toml`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::models::Track;
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::playback::{NowPlaying, PlayState};
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

/// Une zone SANS sortie enregistrée : l'orchestrateur ne cherche alors aucun
/// périphérique et la bascule s'éprouve pour ce qu'elle décide, pas pour ce
/// qu'un pilote audio absent aurait refusé.
fn zone(state: &AppState, nom: &str) -> i64 {
    ZoneRepo::with_backend(state.backend.clone())
        .create(nom, Some("browser"), None)
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

fn en_cours(titre: &str, track_id: Option<i64>) -> NowPlaying {
    NowPlaying {
        track_id,
        title: titre.into(),
        duration_ms: 240_000,
        source: "local".into(),
        ..Default::default()
    }
}

// --- 1 & 2 : la bascule bascule ---------------------------------------

#[tokio::test]
async fn en_lecture_la_bascule_met_en_pause() {
    let (app, state) = app_et_etat();
    let z = zone(&state, "Salon");
    let t = piste(&state, "Kind of Blue");
    state
        .playback
        .play(z, en_cours("Kind of Blue", Some(t)))
        .await;
    assert_eq!(state.playback.get_state(z).await.state, PlayState::Playing);

    let (code, _) = poster(&app, &format!("/api/v1/zones/{z}/playpause")).await;
    assert_eq!(
        code,
        StatusCode::OK,
        "la bascule doit aboutir sur une zone en lecture"
    );
    assert_eq!(
        state.playback.get_state(z).await.state,
        PlayState::Paused,
        "un appui sur KEY_PLAYPAUSE pendant la lecture met en pause"
    );
}

#[tokio::test]
async fn en_pause_la_bascule_reprend() {
    let (app, state) = app_et_etat();
    let z = zone(&state, "Salon");
    let t = piste(&state, "Kind of Blue");
    state
        .playback
        .play(z, en_cours("Kind of Blue", Some(t)))
        .await;
    state.playback.pause(z).await;
    assert_eq!(state.playback.get_state(z).await.state, PlayState::Paused);

    let (code, _) = poster(&app, &format!("/api/v1/zones/{z}/playpause")).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        state.playback.get_state(z).await.state,
        PlayState::Playing,
        "le second appui sur la MÊME touche doit repartir"
    );
}

/// Deux appuis de suite reviennent à l'état de départ — c'est la définition
/// d'une bascule, et c'est ce que le bouton physique promet à la main.
#[tokio::test]
async fn deux_appuis_reviennent_a_l_etat_de_depart() {
    let (app, state) = app_et_etat();
    let z = zone(&state, "Salon");
    let t = piste(&state, "So What");
    state.playback.play(z, en_cours("So What", Some(t))).await;

    poster(&app, &format!("/api/v1/zones/{z}/playpause")).await;
    poster(&app, &format!("/api/v1/zones/{z}/playpause")).await;

    assert_eq!(
        state.playback.get_state(z).await.state,
        PlayState::Playing,
        "pause puis reprise : on est revenu en lecture"
    );
}

// --- 3 : ⭐ le cas dangereux du bouton physique ------------------------

/// Une zone à l'arrêt, sans piste en mémoire et sans file : la bascule refuse
/// et **n'écrit rien**.
///
/// Sans cette garde, `resume` traverse toutes ses branches sans rien trouver et
/// finit quand même par `playback.resume()` : la zone serait annoncée « en
/// lecture » alors que rien ne joue. Derrière une touche de télécommande,
/// qu'on presse sans regarder l'écran, c'est un mensonge d'état.
#[tokio::test]
async fn zone_a_l_arret_et_vide_la_bascule_refuse_sans_rien_ecrire() {
    let (app, state) = app_et_etat();
    let z = zone(&state, "Chambre");
    assert_eq!(state.playback.get_state(z).await.state, PlayState::Stopped);

    let (code, corps) = poster(&app, &format!("/api/v1/zones/{z}/playpause")).await;

    assert_eq!(
        code,
        StatusCode::CONFLICT,
        "rien à jouer : la bascule le DIT, elle ne fait pas semblant ; corps : {corps}"
    );
    assert_eq!(corps["code"].as_str(), Some("zone_vide"));
    assert_eq!(
        state.playback.get_state(z).await.state,
        PlayState::Stopped,
        "⚠️ la zone doit rester à l'arrêt : un refus qui écrit n'est pas un refus"
    );
}

/// CONTRE-ÉPREUVE du témoin ci-dessus : ce n'est pas « à l'arrêt ⇒ refus ».
/// La même zone, à l'arrêt elle aussi, mais avec une FILE : elle repart.
#[tokio::test]
async fn zone_a_l_arret_avec_une_file_repart() {
    let (app, state) = app_et_etat();
    let z = zone(&state, "Chambre");
    let t = piste(&state, "Blue in Green");
    PlayQueueRepo::with_backend(state.backend.clone())
        .append(z, &[QueueInput::Local { track_id: t }])
        .expect("mise en file");
    assert_eq!(state.playback.get_state(z).await.state, PlayState::Stopped);

    let (code, corps) = poster(&app, &format!("/api/v1/zones/{z}/playpause")).await;

    assert_ne!(
        code,
        StatusCode::CONFLICT,
        "une file non vide n'est PAS « rien à jouer » : {corps}"
    );
}

/// Et la seconde moitié de la contre-épreuve : à l'arrêt, file vide, mais une
/// piste encore en mémoire (le cas d'un `stop` qui garde `now_playing`) — la
/// bascule ne refuse pas non plus, `resume` sait la rejouer (#2876).
#[tokio::test]
async fn zone_a_l_arret_avec_une_piste_en_memoire_ne_refuse_pas() {
    let (app, state) = app_et_etat();
    let z = zone(&state, "Chambre");
    let t = piste(&state, "Flamenco Sketches");
    state
        .playback
        .play(z, en_cours("Flamenco Sketches", Some(t)))
        .await;
    state.playback.stop(z).await;
    let avant = state.playback.get_state(z).await;
    assert_eq!(avant.state, PlayState::Stopped);
    assert!(
        avant.now_playing.is_some(),
        "préalable du témoin : `stop` garde la piste en mémoire"
    );

    let (code, corps) = poster(&app, &format!("/api/v1/zones/{z}/playpause")).await;

    assert_ne!(
        code,
        StatusCode::CONFLICT,
        "une piste en mémoire n'est PAS « rien à jouer » : {corps}"
    );
}

// --- 5 : la radio ------------------------------------------------------

/// Une RADIO en cours : la bascule met en pause comme pour une piste, et elle
/// ne touche PAS la file — elle délègue à `pause`, elle ne réimplémente rien.
///
/// Le point mérite son témoin : #3342 a montré qu'un geste de transport mal
/// posé sur une radio pouvait ressusciter la file de l'écoute précédente.
#[tokio::test]
async fn sur_une_radio_la_bascule_met_en_pause_et_ne_touche_pas_la_file() {
    let (app, state) = app_et_etat();
    let z = zone(&state, "Cuisine");

    // Une file héritée d'une écoute précédente, comme chez Philippe (#3342).
    let t = piste(&state, "Simplifier");
    PlayQueueRepo::with_backend(state.backend.clone())
        .append(z, &[QueueInput::Local { track_id: t }])
        .expect("mise en file");
    state.playback.update_queue_info(z, 0, 1).await;

    // La station telle que `play_radio` la déclare : source `radio`, l'URL en
    // `source_id`, aucune durée — une radio n'a pas de fin.
    let station = NowPlaying {
        track_id: None,
        title: "FIP Jazz".into(),
        duration_ms: 0,
        source: "radio".into(),
        source_id: Some("http://icecast.radiofrance.fr/fipjazz-hifi.aac".into()),
        ..Default::default()
    };
    state.playback.play(z, station).await;

    let (code, _) = poster(&app, &format!("/api/v1/zones/{z}/playpause")).await;

    assert_eq!(code, StatusCode::OK);
    let apres = state.playback.get_state(z).await;
    assert_eq!(
        apres.state,
        PlayState::Paused,
        "une radio se met en pause comme le fait déjà le bouton Pause"
    );
    assert_eq!(
        apres.queue_position, 0,
        "la bascule ne fait pas avancer la file (#3342)"
    );
    assert_eq!(
        apres.now_playing.as_ref().map(|np| np.source.as_str()),
        Some("radio"),
        "et elle ne remplace pas la station par la piste de la file"
    );
}
