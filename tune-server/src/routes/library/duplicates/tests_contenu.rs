//! BIB-B2 (phase C) : `by_content` dans `GET /library/duplicates`.

use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;
use tune_core::audio::empreinte::{TAUX, empreinte_des_echantillons};
use tune_core::db::backend::ToSqlValue;

type Etat = crate::state::AppState;

fn serveur() -> (axum::Router, Etat) {
    let state = Etat::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    (routeur, state)
}

/// Un signal mono 16 bits à [`TAUX`], comme dans les témoins de l'empreinte.
pub(crate) fn signal(frequences: &[f64], secondes: f64, amplitude: f64) -> Vec<i32> {
    let n = (secondes * TAUX as f64) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / TAUX as f64;
            let enveloppe = 0.6 + 0.4 * (2.0 * std::f64::consts::PI * 0.5 * t).sin();
            let x: f64 = frequences
                .iter()
                .map(|f| (2.0 * std::f64::consts::PI * f * t).sin())
                .sum::<f64>()
                / frequences.len() as f64;
            (x * enveloppe * amplitude * 32_767.0) as i32
        })
        .collect()
}

pub(crate) fn empreinte_texte(echantillons: &[i32]) -> String {
    empreinte_des_echantillons(echantillons, 16)
        .unwrap()
        .serialiser()
}

fn piste(state: &Etat, id: i64, titre: &str, chemin: &str, format: &str, empreinte: Option<&str>) {
    state
        .backend
        .execute(
            "INSERT INTO tracks (id, title, artist_id, file_path, duration_ms, format, sample_rate, bit_depth) \
             VALUES (?, ?, 1, ?, 30000, ?, 44100, 16)",
            &[&id as &dyn ToSqlValue, &titre, &chemin, &format],
        )
        .unwrap();
    if let Some(e) = empreinte {
        state
            .backend
            .execute(
                "UPDATE tracks SET audio_fingerprint = ? WHERE id = ?",
                &[&e as &dyn ToSqlValue, &id],
            )
            .unwrap();
    }
}

#[tokio::test]
async fn les_doublons_par_contenu_reunissent_deux_encodages_et_ignorent_le_reste() {
    let (app, state) = serveur();
    state
        .backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Pink Floyd')",
            &[],
        )
        .unwrap();
    let time = signal(&[220.0, 330.0, 440.0], 30.0, 0.8);
    let time_attenue: Vec<i32> = time.iter().map(|s| s / 2).collect();
    let money = signal(&[1_000.0, 1_500.0], 30.0, 0.8);
    piste(
        &state,
        10,
        "Time",
        "/m/dsotm/04 Time.aiff",
        "aiff",
        Some(&empreinte_texte(&time)),
    );
    piste(
        &state,
        11,
        "Time",
        "/m/autre/time.m4a",
        "aac",
        Some(&empreinte_texte(&time_attenue)),
    );
    piste(
        &state,
        12,
        "Money",
        "/m/dsotm/05 Money.aiff",
        "aiff",
        Some(&empreinte_texte(&money)),
    );
    piste(&state, 13, "Sans empreinte", "/m/x.flac", "flac", None);
    piste(
        &state,
        14,
        "Marque",
        "/m/y.flac",
        "flac",
        Some("env100ms-v1:-"),
    );

    let reponse = app
        .oneshot(
            Request::get("/api/v1/library/duplicates")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap();
    let par_contenu = corps["duplicates"]["by_content"]
        .as_array()
        .expect("liste by_content");
    assert_eq!(par_contenu.len(), 1, "{corps}");
    assert_eq!(par_contenu[0]["match_type"], "audio_content");
    let ids: Vec<i64> = par_contenu[0]["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["id"].as_i64())
        .collect();
    assert_eq!(
        ids,
        vec![10, 11],
        "les deux encodages de « Time », et rien d'autre"
    );
    assert_eq!(par_contenu[0]["tracks"][1]["format"], "aac");
    assert!(corps["total"].as_u64().unwrap() >= 1);
}
