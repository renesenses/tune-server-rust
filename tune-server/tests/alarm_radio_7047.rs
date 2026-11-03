//! #7047 — un réveil posé sur une radio ne jouait rien.
//!
//! alec_eiffel (1.0.0-rc7, macOS) écoutait FIP et a posé un réveil à 21:31
//! depuis l'écran de lecture. Journal joint :
//!
//! ```text
//! 21:31:19 alarm_triggered alarm_id=3 name="Alarm" zones=[28] source="playlist:"
//!          play_resolution_failed … error=unknown service: playlist
//!          alarm_play_error alarm_id=3 zone_id=28 error=unknown service: playlist
//! ```
//!
//! La cause : l'écran de lecture (web `api.setAlarm`, Flutter `setAlarm`)
//! envoie `POST /zones/{id}/alarm` avec `{"time": "21:31"}` et rien d'autre.
//! La route écrivait la nature `playlist` par défaut et un identifiant vide.
//!
//! Ce que ce fichier cloue :
//! 1. le réveil posé depuis l'écran de lecture d'une zone qui joue FIP
//!    enregistre FIP, et quand il sonne, c'est le flux de FIP qui est
//!    demandé ;
//! 2. une source vide ou mal formée est refusée en 400 à l'enregistrement,
//!    sur `POST /zones/{id}/alarm`, `POST /alarms` et `PUT /alarms/{id}`, et
//!    rien n'est écrit.
use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use serde_json::{Value, json};
use std::time::Duration;
use tower::ServiceExt;
use tune_core::alarms::AlarmScheduler;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::playback::NowPlaying;
use tune_server::state::AppState;

const FIP: &str = "https://icecast.radiofrance.fr/fip-hifi.aac";

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

async fn etat() -> AppState {
    let s = AppState::new(":memory:", 0, Default::default()).unwrap();
    s.license.set_account_premium(true, None).await;
    s
}

fn zone(s: &AppState) -> i64 {
    ZoneRepo::with_backend(s.backend.clone())
        .create("Salon", Some("dlna"), None)
        .unwrap()
}

fn radio(titre: &str, url: &str) -> NowPlaying {
    NowPlaying {
        title: titre.into(),
        source: "radio".into(),
        source_id: Some(url.into()),
        ..Default::default()
    }
}

/// La source telle que le planificateur la lit et la journalise
/// (`alarm_triggered … source="<nature>:<identifiant>"`).
fn source_du_planificateur(s: &AppState, id: i64) -> String {
    let a = AlarmScheduler::with_backend(s.backend.clone(), s.orchestrator.clone())
        .get_alarm(id)
        .unwrap()
        .expect("le réveil se relit");
    format!(
        "{}:{}",
        a["source_type"].as_str().unwrap_or(""),
        a["source_id"].as_str().unwrap_or("")
    )
}

fn nombre_de_reveils(s: &AppState) -> i64 {
    s.backend
        .query_one("SELECT COUNT(*) FROM alarms", &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap()
}

// ─── 1. Le réveil de l'écran de lecture garde la radio ─────────

#[tokio::test]
async fn i7047_reveil_pose_depuis_l_ecran_de_lecture_sur_fip_enregistre_fip() {
    let s = etat().await;
    let z = zone(&s);
    s.playback.play(z, radio("FIP", FIP)).await;

    // Exactement le corps de `api.setAlarm(zone.id, alarmTime)`.
    let (st, v) = request(
        &s,
        Method::POST,
        &format!("/api/v1/zones/{z}/alarm"),
        json!({"time": "21:31"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let id = v["id"].as_i64().unwrap();

    assert_eq!(
        source_du_planificateur(&s, id),
        format!("radio:{FIP}"),
        "#7047 : le réveil posé en écoutant FIP doit jouer FIP (le testeur a eu « playlist: »)"
    );
    let nom = s
        .backend
        .query_one(
            "SELECT source_name FROM alarms WHERE id = ?",
            &[&id as &dyn tune_core::db::backend::ToSqlValue],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()));
    assert_eq!(nom.as_deref(), Some("FIP"));
}

/// Un faux serveur de radio local, qui répond un flux MP3 minimal : si la
/// lecture va chercher le flux, rien ne sort sur Internet.
async fn station_locale() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/fip-hifi.mp3", ecoute.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut c, _)) = ecoute.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let _ = c.read(&mut buf).await;
                let _ = c
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\n\r\n")
                    .await;
                // Trames MPEG-1 couche III vides : de quoi passer pour un flux.
                let mut trame = vec![0u8; 417];
                trame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
                for _ in 0..200 {
                    if c.write_all(&trame).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    url
}

#[tokio::test]
async fn i7047_le_reveil_radio_de_l_ecran_de_lecture_joue_la_radio_quand_il_sonne() {
    let s = etat().await;
    const SORTIE: &str = "dlna:uuid-salon-7047";
    let z = ZoneRepo::with_backend(s.backend.clone())
        .create("Salon", Some("dlna"), Some(SORTIE))
        .unwrap();
    s.outputs.lock().await.register(Box::new(
        tune_core::outputs::mock::MockOutput::new(SORTIE, "Salon").with_type("dlna"),
    ));
    let url = station_locale().await;
    s.playback.play(z, radio("FIP", &url)).await;
    let (st, v) = request(
        &s,
        Method::POST,
        &format!("/api/v1/zones/{z}/alarm"),
        json!({"time": "21:31"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let id = v["id"].as_i64().unwrap();
    // L'utilisateur coupe la radio et va se coucher : à 21:31 la zone ne joue
    // plus, le réveil y sonne (#5669 : il saute une zone qui joue).
    s.playback.stop(z).await;
    let avant = s.playback.get_state(z).await.track_generation;

    let planificateur = AlarmScheduler::with_backend(s.backend.clone(), s.orchestrator.clone());
    let reveil = planificateur.get_alarm(id).unwrap().unwrap();
    let zones = tokio::time::timeout(Duration::from_secs(30), planificateur.fire_alarm(&reveil))
        .await
        .expect("le déclenchement rend la main");
    assert_eq!(zones.sonnees, vec![z], "{zones:?}");

    // `track_generation` ne bouge que si une lecture a VRAIMENT été lancée :
    // le « playlist: » du testeur échouait avant, la zone restait muette.
    let apres = s.playback.get_state(z).await;
    let joue = apres
        .now_playing
        .as_ref()
        .map(|np| (np.source.clone(), np.source_id.clone().unwrap_or_default()));
    assert!(
        apres.track_generation > avant,
        "#7047 : le réveil a sonné sans rien lancer sur la zone (source {}, lecture {joue:?})",
        source_du_planificateur(&s, id)
    );
    assert_eq!(
        joue,
        Some(("radio".to_string(), url.clone())),
        "#7047 : la zone ne joue pas la radio du réveil"
    );
}

// ─── 2. Une source vide ou mal formée est refusée en 400 ───────

#[tokio::test]
async fn i7047_reveil_de_zone_sans_source_et_sans_radio_refuse_en_400() {
    let s = etat().await;
    let z = zone(&s);
    // Zone muette, puis zone qui joue autre chose qu'une radio.
    for en_cours in [
        None,
        Some(NowPlaying {
            title: "Entreat".into(),
            source: "local".into(),
            track_id: Some(1),
            ..Default::default()
        }),
    ] {
        if let Some(np) = en_cours.clone() {
            s.playback.play(z, np).await;
        }
        let (st, v) = request(
            &s,
            Method::POST,
            &format!("/api/v1/zones/{z}/alarm"),
            json!({"time": "21:31"}),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "#7047 : réveil sans source accepté ({en_cours:?}) → {v}"
        );
        assert_eq!(v["error"], "invalid_source", "{v}");
    }
    assert_eq!(nombre_de_reveils(&s), 0, "rien n'a été écrit");
}

#[tokio::test]
async fn i7047_reveil_de_zone_source_nommee_controlee() {
    let s = etat().await;
    let z = zone(&s);
    let chemin = format!("/api/v1/zones/{z}/alarm");
    for corps in [
        json!({"time": "07:00", "source_type": "playlist"}),
        json!({"time": "07:00", "source_type": "playlist", "source_id": ""}),
        json!({"time": "07:00", "source_type": "radio", "source_id": "FIP"}),
        json!({"time": "07:00", "source_type": "radio"}),
        json!({"time": "07:00", "source_id": FIP}),
        json!({"time": "07:00", "source_type": "podcast", "source_id": FIP}),
    ] {
        let (st, v) = request(&s, Method::POST, &chemin, corps.clone()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{corps} → {v}");
        assert_eq!(v["error"], "invalid_source", "{v}");
    }
    assert_eq!(nombre_de_reveils(&s), 0, "rien n'a été écrit");

    for (corps, attendu) in [
        (
            json!({"time": "07:00", "source_type": "radio", "source_id": FIP}),
            format!("radio:{FIP}"),
        ),
        (
            json!({"time": "07:00", "source_type": "playlist", "source_id": 12}),
            "playlist:12".to_string(),
        ),
        (
            json!({"time": "07:00", "playlist_id": 12}),
            "playlist:12".to_string(),
        ),
        (
            json!({"time": "07:00", "album_id": 7}),
            "album:7".to_string(),
        ),
    ] {
        let (st, v) = request(&s, Method::POST, &chemin, corps.clone()).await;
        assert_eq!(st, StatusCode::CREATED, "{corps} → {v}");
        let id = v["id"].as_i64().unwrap();
        assert_eq!(source_du_planificateur(&s, id), attendu, "{corps}");
    }
}

fn reveil_global(source_type: Value, source_id: Value) -> Value {
    let mut b = json!({ "name": "Réveil", "time": "21:31", "days": "daily", "volume": 0.5 });
    if !source_type.is_null() {
        b["source_type"] = source_type;
    }
    if !source_id.is_null() {
        b["source_id"] = source_id;
    }
    b
}

#[tokio::test]
async fn i7047_post_alarms_source_vide_ou_mal_formee_refuse_en_400() {
    let s = etat().await;
    for corps in [
        reveil_global(Value::Null, Value::Null),
        reveil_global(json!("playlist"), Value::Null),
        reveil_global(json!("playlist"), json!("")),
        reveil_global(json!("radio"), json!("")),
        reveil_global(json!("radio"), json!("FIP")),
        reveil_global(json!("radio"), json!("stream.fip.fr/fip.aac")),
        reveil_global(json!(""), json!(FIP)),
        reveil_global(json!("podcast"), json!(FIP)),
    ] {
        let (st, v) = request(&s, Method::POST, "/api/v1/alarms", corps.clone()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "#7047 : {corps} accepté → {v}");
        assert_eq!(v["error"], "invalid_source", "{v}");
    }
    assert_eq!(nombre_de_reveils(&s), 0, "rien n'a été écrit");

    let (st, v) = request(
        &s,
        Method::POST,
        "/api/v1/alarms",
        reveil_global(json!("radio"), json!(format!(" {FIP} "))),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let id = v["id"].as_i64().unwrap();
    assert_eq!(source_du_planificateur(&s, id), format!("radio:{FIP}"));
}

#[tokio::test]
async fn i7047_put_alarms_source_mal_formee_refuse_en_400_et_n_ecrit_rien() {
    let s = etat().await;
    let (st, v) = request(
        &s,
        Method::POST,
        "/api/v1/alarms",
        reveil_global(json!("radio"), json!(FIP)),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let id = v["id"].as_i64().unwrap();
    let chemin = format!("/api/v1/alarms/{id}");
    for corps in [
        json!({"source_id": ""}),
        json!({"source_id": "FIP"}),
        json!({"source_type": "playlist", "source_id": "  "}),
        json!({"source_type": "podcast"}),
    ] {
        let (st, v) = request(&s, Method::PUT, &chemin, corps.clone()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{corps} → {v}");
        assert_eq!(v["error"], "invalid_source", "{v}");
    }
    assert_eq!(
        source_du_planificateur(&s, id),
        format!("radio:{FIP}"),
        "rien n'a été écrit"
    );
    // Changer de radio reste possible.
    let autre = "https://icecast.radiofrance.fr/fipjazz-hifi.aac";
    let (st, v) = request(&s, Method::PUT, &chemin, json!({"source_id": autre})).await;
    assert!(st.is_success(), "{st}: {v}");
    assert_eq!(source_du_planificateur(&s, id), format!("radio:{autre}"));
}

/// Un réveil déjà enregistré « playlist: » (avant ce correctif) se désactive
/// encore : l'écran renvoie le réveil complet, source inchangée comprise.
#[tokio::test]
async fn i7047_put_source_ancienne_inchangee_ne_bloque_pas_l_interrupteur() {
    use tune_core::db::backend::ToSqlValue;
    let s = etat().await;
    let id = s
        .backend
        .execute_returning_id(
            "INSERT INTO alarms (name, time, source_type, source_id, enabled) VALUES ('Alarm', '21:31', 'playlist', NULL, 1)",
            &[],
        )
        .unwrap();
    let (st, v) = request(
        &s,
        Method::PUT,
        &format!("/api/v1/alarms/{id}"),
        json!({"source_type": "playlist", "enabled": false}),
    )
    .await;
    assert!(st.is_success(), "{st}: {v}");
    let enabled = s
        .backend
        .query_one(
            "SELECT enabled FROM alarms WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()));
    assert_eq!(enabled, Some(0));
}
