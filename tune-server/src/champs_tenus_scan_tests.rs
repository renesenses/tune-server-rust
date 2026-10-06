//! 05/10/2026 — « modifications tenues » : une piste corrigée dans Tune, sans
//! écriture dans le fichier (le défaut), garde sa correction à travers une
//! VRAIE « Analyse complète » (`spawn_library_scan`, forcé) ; « Rétablir
//! depuis le fichier » (`DELETE …/tenues`) rend la valeur des balises, et
//! l'analyse suivante ne la défait pas.
use super::surveillant_retouche_tests_4896::{baliser, flac_8_canaux};
use crate::state::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::time::{Duration, Instant, SystemTime};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

#[allow(clippy::await_holding_lock)]
async fn scan_force(etat: &AppState) {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
    let mut rx = etat.event_bus.subscribe();
    let debut = Instant::now();
    while !crate::routes::system::scan::spawn_library_scan(etat.clone(), true, None).await {
        assert!(
            debut.elapsed() < Duration::from_secs(120),
            "pas de droit de scanner"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let fin = tune_core::event_types::EventType::ScanComplete.as_str();
    loop {
        match tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("le scan forcé n'a pas annoncé sa fin")
        {
            Ok(ev) if ev.event_type == fin => {
                crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
                return;
            }
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(e) => panic!("bus fermé : {e}"),
        }
    }
}

async fn appel(etat: &AppState, methode: &str, chemin: &str, corps: Value) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&corps).unwrap()))
        .unwrap();
    let reponse = crate::routes::router(etat.clone())
        .oneshot(requete)
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

/// `(id, titre, genre, année, compositeur)` de l'unique piste locale.
fn ligne(etat: &AppState) -> (i64, String, Option<String>, Option<i64>, Option<String>) {
    let r = etat
        .backend
        .query_one(
            "SELECT id, title, genre, year, composer FROM tracks WHERE file_path IS NOT NULL",
            &[],
        )
        .unwrap()
        .expect("la piste n'a pas été analysée");
    (
        r[0].as_i64().unwrap(),
        r[1].as_string().unwrap_or_default(),
        r[2].as_string(),
        r[3].as_i64(),
        r[4].as_string(),
    )
}

#[tokio::test]
async fn edition_puis_analyse_complete_la_correction_tient_puis_retablir() {
    let base = tune_core::test_scratch::scratch_dir("champs-tenus-base");
    let racine = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        "champs-tenus-bibliotheque",
    );
    let etat = AppState::new(
        &base.join("tune.db").to_string_lossy(),
        0,
        Default::default(),
    )
    .expect("AppState sur base de fichier");
    SettingsRepo::with_backend(etat.backend.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&[racine.to_string_lossy()]).unwrap(),
        )
        .unwrap();
    let dossier = racine.join("Artiste - Album");
    std::fs::create_dir_all(&dossier).unwrap();
    let piste = dossier.join("01.flac");
    std::fs::write(&piste, flac_8_canaux()).unwrap();
    baliser(
        &piste,
        &[
            ("TITLE", "Titre du fichier"),
            ("ARTIST", "Artiste"),
            ("ALBUM", "Album"),
            ("TRACKNUMBER", "1"),
            ("GENRE", "Rock"),
            ("DATE", "1988"),
            ("COMPOSER", "Balise"),
        ],
        SystemTime::now() - Duration::from_secs(86_400),
    );
    let octets_du_fichier = std::fs::read(&piste).unwrap();

    scan_force(&etat).await;
    let (id, _, genre, annee, _) = ligne(&etat);
    assert_eq!(
        genre.as_deref(),
        Some("Rock"),
        "témoin : le scan lit la balise"
    );
    assert_eq!(annee, Some(1988));

    // L'édition, réglage « écrire dans les fichiers » jamais touché.
    let (statut, corps) = appel(
        &etat,
        "PATCH",
        &format!("/api/v1/metadata/tracks/{id}"),
        json!({"genre": "Jazz", "year": 1999, "composer": "Corrigé à la main"}),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["file_writes_enabled"], false, "{corps}");
    assert_eq!(
        std::fs::read(&piste).unwrap(),
        octets_du_fichier,
        "fichier réécrit"
    );

    // L'analyse complète relit TOUTES les balises : la correction tient.
    scan_force(&etat).await;
    let (id, titre, genre, annee, compositeur) = ligne(&etat);
    assert_eq!(
        genre.as_deref(),
        Some("Jazz"),
        "l'analyse complète a défait le genre"
    );
    assert_eq!(annee, Some(1999), "l'analyse complète a défait l'année");
    assert_eq!(compositeur.as_deref(), Some("Corrigé à la main"));
    assert_eq!(
        titre, "Titre du fichier",
        "un champ non édité suit la balise"
    );

    let (statut, tenus) = appel(
        &etat,
        "GET",
        &format!("/api/v1/library/tracks/{id}/tenues"),
        Value::Null,
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    let noms: Vec<&str> = tenus["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(noms, vec!["genre", "year", "composer"], "{tenus}");

    // « Rétablir depuis le fichier ».
    let (statut, corps) = appel(
        &etat,
        "DELETE",
        &format!("/api/v1/library/tracks/{id}/tenues"),
        Value::Null,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let (_, _, genre, annee, compositeur) = ligne(&etat);
    assert_eq!(
        genre.as_deref(),
        Some("Rock"),
        "rétablir n'a pas relu le fichier"
    );
    assert_eq!(annee, Some(1988));
    assert_eq!(compositeur.as_deref(), Some("Balise"));

    scan_force(&etat).await;
    let (_, _, genre, _, _) = ligne(&etat);
    assert_eq!(
        genre.as_deref(),
        Some("Rock"),
        "une tenue effacée est revenue"
    );
}
