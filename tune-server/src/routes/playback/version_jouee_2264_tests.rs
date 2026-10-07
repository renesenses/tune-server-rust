//! #2264 — `POST /zones/{id}/play` joue la version de la RÈGLE, quel que soit
//! le lancement (piste, liste de pistes, album, playlist), sauf choix
//! explicite ; la règle est celle du profil qui lance ; la piste en cours le
//! publie (`current_track.version`).
//!
//! Sur le vrai routeur, avec une sortie factice et de vrais fichiers FLAC.
//! Le banc : « So What » en 44,1/16 (STANDARD, celle qu'on lance) et en 96/24
//! (HAUTE, même enregistrement), et un « So What (Live) ».

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::state::AppState;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::library::groupes_versions::RegleDeChoix;
use tune_core::library::regle_de_version::poser_regle_du_profil;

const SORTIE: &str = "dlna:banc-2264-route";
const STANDARD: i64 = 1;
const HAUTE: i64 = 2;
const LIVE: i64 = 3;

struct Banc {
    state: AppState,
    zone: i64,
    _dossier: tempfile::TempDir,
}

async fn banc() -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let dossier = tempfile::tempdir().unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tune-core/tests/fixtures/test.flac");
    let mut chemins = Vec::new();
    for nom in ["standard.flac", "haute.flac", "live.flac"] {
        let chemin = dossier.path().join(nom);
        std::fs::copy(&source, &chemin).unwrap();
        chemins.push(chemin.to_string_lossy().into_owned());
    }
    for sql in [
        "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis')",
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Kind of Blue', 1)",
        "INSERT INTO albums (id, title, artist_id) VALUES (2, 'Kind of Blue (Legacy)', 1)",
        "INSERT INTO albums (id, title, artist_id) VALUES (3, 'Newport 1958', 1)",
    ] {
        state.backend.execute(sql, &[]).unwrap();
    }
    for (id, titre, album, duree, sr, bd, chemin) in [
        (
            STANDARD,
            "So What",
            1i64,
            300_000i64,
            44_100i64,
            16i64,
            &chemins[0],
        ),
        (HAUTE, "So What", 2, 300_500, 96_000, 24, &chemins[1]),
        (LIVE, "So What (Live)", 3, 300_200, 44_100, 16, &chemins[2]),
    ] {
        let p: [&dyn ToSqlValue; 7] = [&id, &titre, &album, &duree, &sr, &bd, chemin];
        state
            .backend
            .execute(
                "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, sample_rate, \
                 bit_depth, file_path, format, channels, source, track_number, disc_number) \
                 VALUES (?, ?, ?, 1, ?, ?, ?, ?, 'flac', 2, 'local', 1, 1)",
                &p,
            )
            .unwrap();
    }
    let zones = ZoneRepo::with_backend(state.backend.clone());
    let zone = zones
        .create("Salon 2264", Some("dlna"), Some(SORTIE))
        .unwrap();
    zones.update_dlna_native_flac(zone, true).unwrap();
    state.outputs.lock().await.register(Box::new(
        tune_core::outputs::mock::MockOutput::new(SORTIE, "Salon 2264").with_type("dlna"),
    ));
    Banc {
        state,
        zone,
        _dossier: dossier,
    }
}

async fn appeler(
    b: &Banc,
    methode: &str,
    url: &str,
    profil: Option<i64>,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(url);
    if let Some(p) = profil {
        req = req.header("X-Profile-Id", p.to_string());
    }
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let rep = crate::routes::router(b.state.clone())
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let statut = rep.status();
    let octets = to_bytes(rep.into_body(), usize::MAX).await.unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn lancer(b: &Banc, corps: Value, profil: Option<i64>) -> Value {
    let (statut, rendu) = appeler(
        b,
        "POST",
        &format!("/api/v1/zones/{}/play", b.zone),
        profil,
        Some(corps.clone()),
    )
    .await;
    assert!(statut.is_success(), "{corps} → {statut} {rendu}");
    let (_, zone) = appeler(b, "GET", &format!("/api/v1/zones/{}", b.zone), None, None).await;
    zone["current_track"].clone()
}

#[tokio::test]
async fn chaque_type_de_lancement_joue_la_version_de_la_regle() {
    let b = banc().await;
    let playlist = PlaylistRepo::with_backend(b.state.backend.clone())
        .create("Liste 2264", None, 1)
        .unwrap();
    PlaylistRepo::with_backend(b.state.backend.clone())
        .add_tracks(playlist, &[STANDARD], None)
        .unwrap();

    for corps in [
        json!({ "track_id": STANDARD }),
        json!({ "track_ids": [STANDARD] }),
        json!({ "album_id": 1 }),
        json!({ "playlist_id": playlist }),
    ] {
        // Une autre piste entre deux : chaque lancement part d'ailleurs.
        lancer(&b, json!({ "track_id": LIVE }), None).await;
        let piste = lancer(&b, corps.clone(), None).await;
        assert_eq!(
            piste["track_id"], HAUTE,
            "{corps} : la version de la règle `local`"
        );
        assert_eq!(piste["sample_rate"], 96_000, "{corps} : la qualité JOUÉE");
        assert_eq!(piste["version"]["origin"], "rule", "{corps}");
        assert_eq!(
            piste["version"]["requested"]["track_id"], STANDARD,
            "{corps}"
        );
        assert_eq!(piste["version"]["fallback"], false, "{corps}");
    }
}

#[tokio::test]
async fn le_choix_explicite_du_panneau_prime() {
    let b = banc().await;
    let piste = lancer(
        &b,
        json!({ "track_id": STANDARD, "explicit_version": true }),
        None,
    )
    .await;
    assert_eq!(piste["track_id"], STANDARD);
    assert_eq!(piste["version"]["origin"], "explicit");
    assert_eq!(piste["version"]["requested"], Value::Null);

    // Contre-épreuve : le même corps sans le drapeau.
    lancer(&b, json!({ "track_id": LIVE }), None).await;
    let piste = lancer(&b, json!({ "track_id": STANDARD }), None).await;
    assert_eq!(piste["track_id"], HAUTE);

    // Une LISTE n'est pas un choix de version : le drapeau y est ignoré.
    lancer(&b, json!({ "track_id": LIVE }), None).await;
    let piste = lancer(
        &b,
        json!({ "track_ids": [STANDARD], "explicit_version": true }),
        None,
    )
    .await;
    assert_eq!(piste["track_id"], HAUTE);
}

#[tokio::test]
async fn la_regle_est_celle_du_profil_qui_lance() {
    let b = banc().await;
    let p2 = ProfileRepo::with_backend(b.state.backend.clone())
        .create("profil-2264", None, None)
        .unwrap();
    // Profil 2 : Qobuz d'abord — Qobuz n'est pas connecté sur ce banc.
    poser_regle_du_profil(
        &b.state.backend,
        p2,
        Some(&RegleDeChoix::PrefererService("qobuz".into())),
    )
    .unwrap();

    let piste = lancer(&b, json!({ "track_id": STANDARD }), Some(p2)).await;
    assert_eq!(
        piste["track_id"], HAUTE,
        "repli : bibliothèque, meilleure qualité"
    );
    assert_eq!(piste["version"]["rule"], "service:qobuz");
    assert_eq!(piste["version"]["rule_origin"], "profile");
    assert_eq!(piste["version"]["fallback"], true, "le repli est SIGNALÉ");
    assert_eq!(piste["version"]["unavailable_source"], "qobuz");

    // Contre-épreuve : le profil 1 n'a rien réglé.
    lancer(&b, json!({ "track_id": LIVE }), Some(1)).await;
    let piste = lancer(&b, json!({ "track_id": STANDARD }), Some(1)).await;
    assert_eq!(piste["version"]["rule"], "local");
    assert_eq!(piste["version"]["rule_origin"], "default");
    assert_eq!(piste["version"]["fallback"], false);
}
