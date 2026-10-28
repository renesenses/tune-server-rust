use crate::state::AppState;
use axum::{Router, body::Body, http::Request, routing::get};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::{listening_resume, settings_repo::SettingsRepo, zone_repo::ZoneRepo};

struct Banc {
    state: AppState,
    app: Router,
    zone: i64,
    url: String,
    amont: tokio::task::JoinHandle<()>,
}
impl Drop for Banc {
    fn drop(&mut self) {
        self.amont.abort();
        tune_core::taches_de_fond::priorite::oublier_la_zone(self.zone);
    }
}
async fn banc() -> Banc {
    let n = 360_u32 * 8000 * 2;
    let mut media = b"RIFF".to_vec();
    media.extend((n + 36).to_le_bytes());
    media.extend(b"WAVEfmt ");
    media.extend(16_u32.to_le_bytes());
    media.extend(1_u16.to_le_bytes());
    media.extend(1_u16.to_le_bytes());
    media.extend(8000_u32.to_le_bytes());
    media.extend(16000_u32.to_le_bytes());
    media.extend(2_u16.to_le_bytes());
    media.extend(16_u16.to_le_bytes());
    media.extend(b"data");
    media.extend(n.to_le_bytes());
    media.resize(44 + n as usize, 0);
    let amont = Router::new().route(
        "/episode.wav",
        get(move || {
            let data = media.clone();
            async move { ([(axum::http::header::CONTENT_TYPE, "audio/wav")], data) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/episode.wav", listener.local_addr().unwrap());
    let amont = tokio::spawn(async move {
        axum::serve(listener, amont).await.unwrap();
    });
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Cet ordinateur — témoin", Some("browser"), None)
        .unwrap();
    let app = crate::routes::router(state.clone());
    Banc {
        state,
        app,
        zone,
        url,
        amont,
    }
}
async fn appel(b: &Banc, path: &str, body: Option<Value>) -> Value {
    let req = Request::builder()
        .uri(format!("/api/v1{path}"))
        .method(if body.is_some() { "POST" } else { "GET" })
        .header("content-type", "application/json")
        .body(body.map_or(Body::empty(), |v| Body::from(v.to_string())))
        .unwrap();
    let response = b.app.clone().oneshot(req).await.unwrap();
    let statut = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(statut.is_success(), "{path}: {statut} {v}");
    v
}
async fn lancer(b: &Banc) -> String {
    let r=appel(b,&format!("/podcasts/play/{}",b.zone),Some(json!({"audio_url":b.url,"title":"Épisode silencieux","podcast_name":"Émission témoin","podcast_id":"emission-stable","cover_url":"https://cover.invalid/podcast.jpg","duration_ms":360000}))).await;
    assert!(
        r["stream_url"]
            .as_str()
            .is_some_and(|u| u.contains("/stream/")),
        "le podcast navigateur doit fournir un flux Tune confirmable : {r}"
    );
    let state = b.state.playback.get_state(b.zone).await;
    state
        .now_playing
        .unwrap()
        .stream_id
        .expect("identifiant du flux navigateur")
}
async fn signaler(b: &Banc, sid: &str, position: i64) -> Value {
    appel(
        b,
        &format!("/zones/{}/browser-playing", b.zone),
        Some(json!({"stream_id":sid,"position_ms":position})),
    )
    .await
}
fn ecoutes(b: &Banc) -> i64 {
    b.state
        .backend
        .query_one("SELECT COUNT(*) FROM listen_history", &[])
        .unwrap()
        .unwrap()[0]
        .as_i64()
        .unwrap()
}
#[tokio::test]
async fn podcast_navigateur_7354_confirme_position_identite_et_une_seule_ecoute() {
    let b = banc().await;
    let sid = lancer(&b).await;
    assert!(
        listening_resume::recent(&b.state.backend, Some(b.zone), 10)
            .unwrap()
            .is_empty(),
        "aucune reprise avant la preuve du lecteur"
    );
    assert_eq!(ecoutes(&b), 0);
    signaler(&b, &sid, 0).await;
    assert!(
        listening_resume::recent(&b.state.backend, Some(b.zone), 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(ecoutes(&b), 0);
    assert_eq!(signaler(&b, &sid, 320000).await["status"], "confirmed");
    let r = listening_resume::recent(&b.state.backend, Some(b.zone), 10)
        .unwrap()
        .remove(0);
    assert_eq!(r.position_ms, 320000);
    assert_eq!(r.context_id, "emission-stable");
    assert_eq!(r.title, "Émission témoin");
    assert_eq!(r.last_track_title, "Épisode silencieux");
    assert_eq!(
        r.cover_path.as_deref(),
        Some("https://cover.invalid/podcast.jpg")
    );
    assert_eq!(ecoutes(&b), 1);
    signaler(&b, "flux-perime", 990000).await;
    assert_eq!(
        listening_resume::load(&b.state.backend, &r.resume_key)
            .unwrap()
            .unwrap()
            .position_ms,
        320000
    );
    signaler(&b, &sid, 325000).await;
    let suivant = listening_resume::load(&b.state.backend, &r.resume_key)
        .unwrap()
        .unwrap();
    assert_eq!(suivant.position_ms, 325000);
    assert_eq!(ecoutes(&b), 1);
    b.state.orchestrator.stop(b.zone, None).await;
}
#[tokio::test]
async fn podcast_navigateur_7354_recul_10_0_300_et_reprises_explicites_sans_double_appui() {
    let b = banc().await;
    let mut sid = lancer(&b).await;
    let settings = SettingsRepo::with_backend(b.state.backend.clone());
    for (position, recul, attendu) in [
        (320000, 10, 310000),
        (320000, 0, 320000),
        (320000, 300, 20000),
        (5000, 10, 0),
    ] {
        signaler(&b, &sid, position).await;
        let r = listening_resume::recent(&b.state.backend, Some(b.zone), 10)
            .unwrap()
            .remove(0);
        settings
            .set(listening_resume::REWIND_KEY, &recul.to_string())
            .unwrap();
        let retour = appel(
            &b,
            &format!("/home/continue-listening/{}/resume", b.zone),
            Some(json!({"resume_key":r.resume_key})),
        )
        .await;
        assert_eq!(
            retour["position_ms"], attendu,
            "position={position}, recul={recul}"
        );
        let state = b.state.playback.get_state(b.zone).await;
        let nouvelle = state.now_playing.unwrap().stream_id.unwrap();
        assert_ne!(
            nouvelle, sid,
            "une reprise explicite ne doit pas être absorbée comme double appui"
        );
        sid = nouvelle;
    }
    b.state.orchestrator.stop(b.zone, None).await;
}

#[tokio::test]
async fn podcast_navigateur_7354_la_premiere_position_cree_la_reprise_sans_attendre_le_poller() {
    let b = banc().await;
    b.state
        .playback
        .set_session_context(
            b.zone,
            Some("podcast".into()),
            Some("emission-stable".into()),
            Some("podcast".into()),
            Some("Émission".into()),
            None,
        )
        .await;
    b.state
        .playback
        .play(
            b.zone,
            tune_core::playback::NowPlaying {
                source: "podcast".into(),
                source_id: Some(b.url.clone()),
                stream_id: Some("flux-courant".into()),
                title: "Épisode".into(),
                ..Default::default()
            },
        )
        .await;
    b.state
        .orchestrator
        .signaler_lecture_navigateur(b.zone, "flux-courant", 42000)
        .await;
    let rows = listening_resume::recent(&b.state.backend, Some(b.zone), 10).unwrap();
    assert_eq!(
        rows.len(),
        1,
        "la première position du podcast doit créer sa reprise, pas seulement mettre à jour une ligne inexistante"
    );
    assert_eq!(rows[0].position_ms, 42000);
    assert_eq!(rows[0].context_id, "emission-stable");
    b.state.orchestrator.stop(b.zone, None).await;
}
#[tokio::test]
async fn podcast_navigateur_7354_entree_explicite_cree_un_flux_neuf_meme_immediatement() {
    let b = banc().await;
    let demande = || tune_core::orchestrator::PlayRequest {
        zone_id: b.zone,
        source: Some("podcast".into()),
        source_id: Some(b.url.clone()),
        title: Some("Épisode".into()),
        duration_ms: Some(360000),
        ..Default::default()
    };
    let premier = b.state.orchestrator.play(demande()).await.unwrap();
    let second = b
        .state
        .orchestrator
        .play_context_resume(demande())
        .await
        .unwrap();
    assert!(premier.stream_url.is_some());
    assert!(second.stream_url.is_some());
    assert_ne!(
        second.stream_url, premier.stream_url,
        "une reprise de contexte explicite ne doit pas être un re-tap coalescé"
    );
    b.state.orchestrator.stop(b.zone, None).await;
}
