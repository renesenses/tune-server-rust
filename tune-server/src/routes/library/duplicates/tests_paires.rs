//! BIB-B3 : `paires`, `criteres` et `?critere=` dans `GET /library/duplicates`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

use super::tests_contenu::{empreinte_texte, signal};

type Etat = crate::state::AppState;

fn serveur() -> (axum::Router, Etat) {
    let state = Etat::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    (routeur, state)
}

#[allow(clippy::too_many_arguments)]
fn piste(
    state: &Etat,
    id: i64,
    titre: &str,
    chemin: &str,
    format: &str,
    sample_rate: i64,
    bit_depth: i64,
    empreinte: Option<&str>,
) {
    state
        .backend
        .execute(
            "INSERT INTO tracks (id, title, artist_id, file_path, duration_ms, format, sample_rate, bit_depth) \
             VALUES (?, ?, 1, ?, 30000, ?, ?, ?)",
            &[
                &id as &dyn ToSqlValue,
                &titre,
                &chemin,
                &format,
                &sample_rate,
                &bit_depth,
            ],
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

async fn appel(app: axum::Router, chemin: &str) -> (StatusCode, Value) {
    let reponse = app
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Deux copies aux mêmes étiquettes (FLAC 96/24 et MP3) et deux encodages du
/// même enregistrement : la porte rend une paire par critère, nomme le
/// critère, dit ce qu'il autorise, et recommande la meilleure copie par la
/// règle de qualité partagée. Les quatre listes d'origine sont toujours là.
#[tokio::test]
async fn les_paires_portent_leur_critere_et_recommandent_la_meilleure_copie() {
    let (app, state) = serveur();
    state
        .backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Nina Simone')",
            &[],
        )
        .unwrap();
    piste(
        &state,
        1,
        "Feeling Good",
        "/m/a/feeling.flac",
        "flac",
        96000,
        24,
        None,
    );
    piste(
        &state,
        2,
        "Feeling Good",
        "/m/b/feeling.mp3",
        "mp3",
        44100,
        16,
        None,
    );
    let sinnerman = signal(&[220.0, 330.0, 440.0], 30.0, 0.8);
    let attenue: Vec<i32> = sinnerman.iter().map(|s| s / 2).collect();
    piste(
        &state,
        3,
        "Sinnerman",
        "/m/a/sinnerman.aiff",
        "aiff",
        44100,
        16,
        Some(&empreinte_texte(&sinnerman)),
    );
    piste(
        &state,
        4,
        "Sinnerman (autre)",
        "/m/b/sinnerman.m4a",
        "aac",
        44100,
        16,
        Some(&empreinte_texte(&attenue)),
    );

    let (statut, corps) = appel(app, "/api/v1/library/duplicates").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let paires = corps["paires"].as_array().expect("paires");
    assert_eq!(paires.len(), 2, "{corps}");

    let etiquettes = paires
        .iter()
        .find(|p| p["critere"] == "etiquettes_identiques")
        .expect("paire par étiquettes");
    assert_eq!(etiquettes["a"]["id"], 1);
    assert_eq!(etiquettes["b"]["id"], 2);
    assert_eq!(etiquettes["b"]["format"], "mp3");
    assert_eq!(etiquettes["recommandation"]["garder"], 1);
    assert_eq!(etiquettes["recommandation"]["raison"], "meilleure_qualite");
    assert_eq!(etiquettes["suppression_sure"], false);

    let contenu = paires
        .iter()
        .find(|p| p["critere"] == "contenu_identique")
        .expect("paire par contenu");
    assert_eq!(contenu["a"]["id"], 3);
    assert_eq!(contenu["b"]["id"], 4);
    assert_eq!(contenu["recommandation"]["garder"], 3, "l'AIFF bat l'AAC");

    let codes: Vec<&str> = corps["criteres"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["code"].as_str())
        .collect();
    assert_eq!(
        codes,
        [
            "fichier_identique",
            "contenu_identique",
            "empreinte_identique",
            "etiquettes_identiques"
        ]
    );
    // Les listes d'origine ne bougent pas : le client d'aujourd'hui les lit.
    assert_eq!(
        corps["duplicates"]["by_metadata"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        corps["duplicates"]["by_content"].as_array().unwrap().len(),
        1
    );
    assert_eq!(corps["duplicates"]["by_metadata"][0]["dup_format"], "mp3");
}

/// `?critere=` ne garde qu'une famille, dans `paires` comme dans la liste
/// d'origine ; un critère inconnu est refusé avec la liste des codes.
#[tokio::test]
async fn le_critere_demande_filtre_et_un_critere_inconnu_est_refuse() {
    let (app, state) = serveur();
    state
        .backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Nina Simone')",
            &[],
        )
        .unwrap();
    piste(
        &state,
        1,
        "Feeling Good",
        "/m/a/feeling.flac",
        "flac",
        96000,
        24,
        None,
    );
    piste(
        &state,
        2,
        "Feeling Good",
        "/m/b/feeling.mp3",
        "mp3",
        44100,
        16,
        None,
    );

    let (statut, corps) = appel(
        app.clone(),
        "/api/v1/library/duplicates?critere=contenu_identique",
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps["paires"].as_array().unwrap().len(), 0, "{corps}");
    assert_eq!(
        corps["duplicates"]["by_metadata"].as_array().unwrap().len(),
        0
    );

    let (statut, corps) = appel(
        app.clone(),
        "/api/v1/library/duplicates?critere=etiquettes_identiques",
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps["paires"].as_array().unwrap().len(), 1, "{corps}");

    let (statut, corps) = appel(app.clone(), "/api/v1/library/duplicates?critere=tous").await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps["paires"].as_array().unwrap().len(), 1);

    let (statut, corps) = appel(app, "/api/v1/library/duplicates?critere=au_pif").await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    let texte = corps.to_string();
    assert!(texte.contains("critere_inconnu:au_pif"), "{texte}");
    assert!(texte.contains("fichier_identique"), "{texte}");
}
