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

async fn appel(app: &axum::Router, methode: &str, chemin: &str, corps: Vec<u8>) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("Content-Type", "application/octet-stream")
        .body(Body::from(corps))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&octets).unwrap_or(json!(null)))
}

#[tokio::test]
async fn un_zip_remplace_le_jeu_et_un_zip_piege_est_refuse_sans_rien_toucher() {
    let dossier = std::env::temp_dir().join(format!("tune_t09a_{}", std::process::id()));
    std::fs::create_dir_all(&dossier).unwrap();
    // SAFETY : seul test de ce binaire, avant toute tâche.
    unsafe { std::env::set_var("TUNE_DATA_DIR", &dossier) };
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state.license.set_account_premium(true, None).await;
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
    let (status, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/room-correction/ir/upload/{zone}?archive=1&name=living-room.zip"),
        z,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(corps["set_name"], "living-room.zip");
    assert_eq!(corps["imported"].as_array().unwrap().len(), 3);

    let (status, statut) = appel(
        &app,
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
        "POST",
        &format!("/api/v1/room-correction/ir/upload/{zone}?name=piege.zip"),
        piege,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refus}");
    assert_eq!(refus["code"], "archive_refused");
    assert_eq!(refus["reason"], "unsafe_path");
    assert_eq!(refus["message_key"], "api.roomCorrection.archive.unsafe_path");
    assert!(!dossier.parent().unwrap().join("evil.wav").exists());
    let (_, apres) = appel(
        &app,
        "GET",
        &format!("/api/v1/room-correction/ir/status/{zone}"),
        Vec::new(),
    )
    .await;
    assert_eq!(apres["files"], statut["files"], "le jeu en place est intact");
    assert_eq!(apres["set_name"], "living-room.zip");

    std::fs::remove_dir_all(&dossier).ok();
}
