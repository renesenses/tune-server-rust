//! #8666 — un réveil posé depuis Lecture en cours quand la zone joue un titre.
//!
//! Recette rc12 sur le .18 (13/11) : la zone jouait un titre (bibliothèque ou
//! Qobuz) ; le bouton Réveil de Lecture en cours envoie `{"time": "07:00"}`
//! sans source, et le serveur refusait :
//!
//! ```text
//! alarm_source_refused zone_id=10 error=no source: name source_type and
//! source_id, or set the alarm while the zone plays a radio
//! ```
//!
//! Ce que ce fichier cloue, par la vraie route `POST /zones/{id}/alarm` :
//! 1. un titre de la bibliothèque : le réveil enregistre SON album, et quand
//!    il sonne, c'est cet album qui joue (première piste, puis la suite) ;
//! 2. ce qui ne peut pas être rejoué (rien en lecture, titre de service hors
//!    bibliothèque, titre sans album) est refusé en
//!    `400 alarm_source_required`, avec un motif (`reason`) ; rien n'est écrit.
use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use serde_json::{Value, json};
use std::time::Duration;
use tower::ServiceExt;
use tune_core::alarms::AlarmScheduler;
use tune_core::db::models::Track;
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::playback::{NowPlaying, PlayState};
use tune_server::state::AppState;

const SORTIE: &str = "dlna:uuid-salon-8666";

async fn request(s: &AppState, method: Method, path: &str, body: Value) -> (StatusCode, Value) {
    let r = tune_server::routes::router(s.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let bytes = axum::body::to_bytes(r.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes))),
    )
}

struct Banc {
    _dossier: tempfile::TempDir,
    s: AppState,
    zone: i64,
    /// Dans l'ordre de l'album : piste 1, piste 2.
    pistes: [i64; 2],
}

const ALBUM: i64 = 86;

async fn banc() -> Banc {
    let s = AppState::new(":memory:", 0, Default::default()).unwrap();
    s.license.set_account_premium(true, None).await;
    let zone = ZoneRepo::with_backend(s.backend.clone())
        .create("Salon", Some("dlna"), Some(SORTIE))
        .unwrap();
    s.outputs.lock().await.register(Box::new(
        tune_core::outputs::mock::MockOutput::new(SORTIE, "Salon").with_type("dlna"),
    ));
    s.backend
        .execute(
            &format!("INSERT INTO albums (id, title) VALUES ({ALBUM}, 'Disintegration')"),
            &[],
        )
        .unwrap();
    let dossier = tempfile::tempdir().unwrap();
    let repo = TrackRepo::with_backend(s.backend.clone());
    let mut pistes = [0; 2];
    for numero in [2i32, 1] {
        let chemin = dossier.path().join(format!("{numero}.wav"));
        std::fs::write(
            &chemin,
            include_bytes!("../../tune-core/tests/fixtures/wav/ref_16_44100_stereo.wav"),
        )
        .unwrap();
        let mut piste = Track::new(format!("Piste {numero}"));
        piste.album_id = Some(ALBUM);
        piste.track_number = numero;
        piste.disc_number = 1;
        piste.file_path = Some(chemin.to_string_lossy().into_owned());
        piste.format = Some("wav".into());
        piste.duration_ms = 60_000;
        piste.sample_rate = Some(44_100);
        piste.bit_depth = Some(16);
        pistes[(numero - 1) as usize] = repo.create(&piste).unwrap();
    }
    Banc {
        _dossier: dossier,
        s,
        zone,
        pistes,
    }
}

fn source_du_planificateur(s: &AppState, id: i64) -> (String, Option<String>) {
    let a = AlarmScheduler::with_backend(s.backend.clone(), s.orchestrator.clone())
        .get_alarm(id)
        .unwrap()
        .expect("le réveil se relit");
    (
        format!(
            "{}:{}",
            a["source_type"].as_str().unwrap_or(""),
            a["source_id"].as_str().unwrap_or("")
        ),
        a["source_name"].as_str().map(str::to_string),
    )
}

fn nombre_de_reveils(s: &AppState) -> i64 {
    s.backend
        .query_one("SELECT COUNT(*) FROM alarms", &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap()
}

/// La piste 2 de l'album, telle que la zone la joue. `album_id` absent : un
/// client plus ancien (ou une lecture lancée par la file) ne le fournit pas,
/// le serveur relit l'album par la piste.
fn piste_en_cours(b: &Banc, album_id: Option<i64>) -> NowPlaying {
    NowPlaying {
        track_id: Some(b.pistes[1]),
        title: "Piste 2".into(),
        album_title: Some("Disintegration".into()),
        source: "local".into(),
        album_id,
        duration_ms: 60_000,
        ..Default::default()
    }
}

// ─── 1. Un titre de la bibliothèque : le réveil rejoue son album ─

#[tokio::test]
async fn i8666_reveil_pose_sur_un_titre_de_la_bibliotheque_joue_son_album_quand_il_sonne() {
    let b = banc().await;
    let z = b.zone;
    b.s.playback.play(z, piste_en_cours(&b, None)).await;

    // Exactement le corps de `api.setAlarm(zone.id, alarmTime)` d'avant
    // web#2962 : aucune source.
    let (st, v) = request(
        &b.s,
        Method::POST,
        &format!("/api/v1/zones/{z}/alarm"),
        json!({"time": "07:00"}),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::CREATED,
        "#8666 : réveil refusé sur un titre de la bibliothèque → {v}"
    );
    let id = v["id"].as_i64().unwrap();
    assert_eq!(v["source_type"], "album", "{v}");
    assert_eq!(
        source_du_planificateur(&b.s, id),
        (format!("album:{ALBUM}"), Some("Disintegration".to_string()))
    );

    // Le réveil sonne sur une zone muette (#5669) : l'album joue, depuis sa
    // première piste, et la suite est dans la file.
    b.s.playback.stop(z).await;
    let planificateur = AlarmScheduler::with_backend(b.s.backend.clone(), b.s.orchestrator.clone());
    let reveil = planificateur.get_alarm(id).unwrap().unwrap();
    let zones = tokio::time::timeout(Duration::from_secs(30), planificateur.fire_alarm(&reveil))
        .await
        .expect("le déclenchement rend la main");
    assert_eq!(zones.sonnees, vec![z], "{zones:?}");
    let etat = b.s.playback.get_state(z).await;
    assert_eq!(
        etat.state,
        PlayState::Playing,
        "#8666 : le réveil n'a rien joué"
    );
    assert_eq!(etat.now_playing.unwrap().track_id, Some(b.pistes[0]));
    let file = PlayQueueRepo::with_backend(b.s.backend.clone())
        .get_queue(z)
        .unwrap();
    assert_eq!(
        file.iter().map(|t| t.track_id).collect::<Vec<_>>(),
        b.pistes.map(Some)
    );
}

#[tokio::test]
async fn i8666_l_album_publie_par_la_lecture_en_cours_sert_de_source() {
    let b = banc().await;
    let z = b.zone;
    let mut np = piste_en_cours(&b, Some(ALBUM));
    np.track_id = None;
    b.s.playback.play(z, np).await;
    let (st, v) = request(
        &b.s,
        Method::POST,
        &format!("/api/v1/zones/{z}/alarm"),
        json!({"time": "07:00"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let id = v["id"].as_i64().unwrap();
    assert_eq!(
        source_du_planificateur(&b.s, id).0,
        format!("album:{ALBUM}")
    );

    // Le corps de web#2962 (`album_id` envoyé) : même réveil.
    let (st, v) = request(
        &b.s,
        Method::POST,
        &format!("/api/v1/zones/{z}/alarm"),
        json!({"time": "07:00", "album_id": ALBUM}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let id = v["id"].as_i64().unwrap();
    assert_eq!(
        source_du_planificateur(&b.s, id).0,
        format!("album:{ALBUM}")
    );
}

// ─── 2. Ce qui ne peut pas être rejoué : refus explicite ───────

#[tokio::test]
async fn i8666_ce_qui_ne_peut_pas_etre_rejoue_est_refuse_avec_un_motif() {
    let b = banc().await;
    let z = b.zone;
    let cas: [(Option<NowPlaying>, &str, Value); 4] = [
        (None, "nothing_playing", Value::Null),
        (
            Some(NowPlaying {
                title: "Lullaby".into(),
                source: "qobuz".into(),
                source_id: Some("12345".into()),
                ..Default::default()
            }),
            "source_not_replayable",
            json!("qobuz"),
        ),
        (
            Some(NowPlaying {
                title: "Entrée ligne".into(),
                source: "line_in".into(),
                ..Default::default()
            }),
            "source_not_replayable",
            json!("line_in"),
        ),
        (
            // Une piste que la bibliothèque ne connaît pas (ou plus).
            Some(NowPlaying {
                title: "Entreat".into(),
                source: "local".into(),
                track_id: Some(999_999),
                ..Default::default()
            }),
            "track_without_album",
            json!("local"),
        ),
    ];
    for (en_cours, motif, source) in cas {
        b.s.playback.stop_and_clear(z).await;
        if let Some(np) = en_cours.clone() {
            b.s.playback.play(z, np).await;
        }
        let (st, v) = request(
            &b.s,
            Method::POST,
            &format!("/api/v1/zones/{z}/alarm"),
            json!({"time": "07:00"}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{en_cours:?} → {v}");
        // Le contrat que lit web#2962 (`apiError` : `code`, puis `error`).
        assert_eq!(v["code"], "alarm_source_required", "{v}");
        assert_eq!(v["error"], "alarm_source_required", "{v}");
        assert_eq!(v["reason"], motif, "{en_cours:?} → {v}");
        assert_eq!(v["playing_source"], source, "{v}");
        assert!(v["message"].as_str().is_some_and(|m| !m.is_empty()), "{v}");
    }
    assert_eq!(nombre_de_reveils(&b.s), 0, "rien n'a été écrit");
}
