//! TESS T09a — la route d'import d'un jeu de convolution, par le routeur
//! réel : un `.zip` de réponses par cadence remplace le jeu de la zone, le
//! statut décrit les fichiers et le nom du jeu ; une archive piégée est
//! refusée avec un motif stable, sans toucher au jeu en place, et sans rien
//! écrire hors du dossier `ir/`.
//!
//! Aucun son n'est joué.

use std::io::{Cursor, Write};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn wav(canaux: &[Vec<f32>], sr: u32) -> Vec<u8> {
    let n = canaux.iter().map(Vec::len).max().unwrap_or(0);
    let ch = canaux.len() as u16;
    let mut data = Vec::new();
    for i in 0..n {
        for c in canaux {
            data.extend_from_slice(&c.get(i).copied().unwrap_or(0.0).to_le_bytes());
        }
    }
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes());
    w.extend_from_slice(&ch.to_le_bytes());
    w.extend_from_slice(&sr.to_le_bytes());
    w.extend_from_slice(&(sr * u32::from(ch) * 4).to_le_bytes());
    w.extend_from_slice(&(ch * 4).to_le_bytes());
    w.extend_from_slice(&32u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data.len() as u32).to_le_bytes());
    w.extend_from_slice(&data);
    w
}

fn archive(fichiers: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut tampon = Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut tampon);
        for (nom, contenu) in fichiers {
            z.start_file(*nom, zip::write::SimpleFileOptions::default())
                .unwrap();
            z.write_all(contenu).unwrap();
        }
        z.finish().unwrap();
    }
    tampon.into_inner()
}

const SECRET: &str = "secret-t09a";

async fn appel(
    app: &axum::Router,
    jeton: &str,
    methode: &str,
    chemin: &str,
    corps: Vec<u8>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("Content-Type", "application/octet-stream")
        .header("Authorization", format!("Bearer {jeton}"))
        .body(Body::from(corps))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&octets).unwrap_or(json!(null)),
    )
}

async fn appel_json(
    app: &axum::Router,
    jeton: &str,
    chemin: &str,
    corps: Vec<u8>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(chemin)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {jeton}"))
        .body(Body::from(corps))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&octets).unwrap_or(json!(null)),
    )
}

#[tokio::test]
async fn un_zip_remplace_le_jeu_et_un_zip_piege_est_refuse_sans_rien_toucher() {
    let racine = tune_core::test_scratch::scratch_dir("tune-t09a-import");
    let dossier = racine.path().to_path_buf();
    // SAFETY : seul test de ce binaire, avant toute tâche.
    unsafe { std::env::set_var("TUNE_DATA_DIR", &dossier) };
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    reglages.set("auth_enabled", "true").unwrap();
    reglages.set("jwt_secret", SECRET).unwrap();
    let admin = tune_server::auth::sign_jwt(1, "admin", SECRET).unwrap();
    let utilisateur = tune_server::auth::sign_jwt(2, "user", SECRET).unwrap();
    // Une zone réseau : l'import n'a pas de sortie locale à rafraîchir.
    let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("dlna"), Some("dlna:essai"))
        .unwrap();
    let app = tune_server::routes::router(state.clone());

    let ri = |a: f32| vec![a, a / 2.0, a / 4.0];
    let z = archive(&[
        ("living-room/room_44k.wav", wav(&[ri(0.5), ri(0.4)], 44_100)),
        ("living-room/room_48k.wav", wav(&[ri(0.6), ri(0.4)], 48_000)),
        ("living-room/room_96k.wav", wav(&[ri(0.7), ri(0.4)], 96_000)),
    ]);
    let route = format!("/api/v1/room-correction/ir/upload/{zone}?archive=1&name=living-room.zip");
    // Gardes : un profil non administrateur, puis un compte sans Premium,
    // sont refusés, et rien n'est posé.
    let (status, _) = appel(&app, &utilisateur, "POST", &route, z.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "profil non administrateur");
    let (status, _) = appel(&app, &admin, "POST", &route, z.clone()).await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "compte sans Premium");
    assert!(
        !dossier.join("ir").exists(),
        "rien n'est écrit sans les droits"
    );
    // Les routes de retrait et d'options ont les mêmes gardes.
    for (route, corps) in [
        (
            format!("/api/v1/room-correction/ir/clear/{zone}"),
            Vec::new(),
        ),
        (
            format!("/api/v1/room-correction/ir/options/{zone}"),
            br#"{"resample":false}"#.to_vec(),
        ),
    ] {
        let (status, _) = appel_json(&app, &utilisateur, &route, corps.clone()).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{route} : profil non administrateur"
        );
        let (status, _) = appel_json(&app, &admin, &route, corps).await;
        assert_eq!(
            status,
            StatusCode::PAYMENT_REQUIRED,
            "{route} : compte sans Premium"
        );
    }
    state.license.set_account_premium(true, None).await;
    let (status, corps) = appel(&app, &admin, "POST", &route, z).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(corps["set_name"], "living-room.zip");
    assert_eq!(corps["imported"].as_array().unwrap().len(), 3);

    let (status, statut) = appel(
        &app,
        &admin,
        "GET",
        &format!("/api/v1/room-correction/ir/status/{zone}"),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(statut["set_name"], "living-room.zip", "{statut}");
    let fichiers = statut["files"].as_array().unwrap();
    let mut cadences: Vec<u64> = fichiers
        .iter()
        .map(|f| f["sample_rate"].as_u64().unwrap())
        .collect();
    cadences.sort_unstable();
    assert_eq!(cadences, [44_100, 48_000, 96_000]);
    let principal = fichiers.iter().find(|f| f["main"] == true).unwrap();
    assert_eq!(principal["sample_rate"], 48_000, "48 kHz est le principal");
    for f in fichiers {
        let p = f["path"].as_str().unwrap();
        assert!(p.starts_with(dossier.join("ir").to_str().unwrap()), "{p}");
    }

    // Contre-épreuve : une archive piégée (`..`) est refusée, motif stable,
    // et le jeu en place est intact.
    let piege = archive(&[("../../evil.wav", wav(&[ri(0.9)], 48_000))]);
    let (status, refus) = appel(
        &app,
        &admin,
        "POST",
        &format!("/api/v1/room-correction/ir/upload/{zone}?name=piege.zip"),
        piege,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refus}");
    assert_eq!(refus["code"], "archive_refused");
    assert_eq!(refus["reason"], "unsafe_path");
    assert_eq!(
        refus["message_key"],
        "api.roomCorrection.archive.unsafe_path"
    );
    assert!(!dossier.parent().unwrap().join("evil.wav").exists());
    let (_, apres) = appel(
        &app,
        &admin,
        "GET",
        &format!("/api/v1/room-correction/ir/status/{zone}"),
        Vec::new(),
    )
    .await;
    assert_eq!(
        apres["files"], statut["files"],
        "le jeu en place est intact"
    );
    assert_eq!(apres["set_name"], "living-room.zip");

    // « Effacer » retire TOUS les fichiers par cadence de la zone, même à une
    // cadence inusuelle déposée hors du jeu.
    let ir = dossier.join("ir");
    std::fs::write(ir.join(format!("zone_{zone}_sr32000.wav")), b"x").unwrap();
    let (status, _) = appel_json(
        &app,
        &admin,
        &format!("/api/v1/room-correction/ir/clear/{zone}"),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let restants: Vec<String> = std::fs::read_dir(&ir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(&format!("zone_{zone}")))
        .collect();
    assert!(restants.is_empty(), "restent : {restants:?}");

    drop(racine);
}
