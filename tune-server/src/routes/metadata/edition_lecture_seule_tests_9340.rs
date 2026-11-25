//! #9340 — éditer une piste dont le fichier est en lecture seule : un refus
//! explicite (409 `fichier_lecture_seule`, « lecture seule » dans le
//! message), et non un 500 « tag write failed: … (os error 13/30) ».
use super::*;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

#[cfg(unix)]
#[tokio::test]
async fn editer_une_piste_en_lecture_seule_est_refuse_explicitement_9340() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("dossier temporaire");
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tune-core/tests/fixtures/test.flac");
    let cible = dir.path().join("piste.flac");
    std::fs::copy(&source, &cible).expect("copie du FLAC d'épreuve");
    let chemin = cible.to_string_lossy().to_string();

    let state = AppState::new(":memory:", 0, Default::default()).expect("état");
    state
        .backend
        .execute(
            "INSERT INTO tracks (title, format, file_path) VALUES ('Avant', 'flac', ?1)",
            &[&chemin as &dyn ToSqlValue],
        )
        .expect("insertion de la piste");
    let id = state.backend.last_insert_rowid();
    crate::routes::ecriture_fichiers::activer_pour_test(&state.backend);

    std::fs::set_permissions(&cible, std::fs::Permissions::from_mode(0o444)).unwrap();
    if std::fs::OpenOptions::new().write(true).open(&cible).is_ok() {
        // root : les droits ne s'appliquent pas, rien à prouver.
        return;
    }
    let r = crate::routes::metadata::router()
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/tracks/{id}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"title":"Après"}"#))
                .expect("requête"),
        )
        .await
        .expect("réponse");
    std::fs::set_permissions(&cible, std::fs::Permissions::from_mode(0o644)).unwrap();
    let code = r.status();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .expect("corps");
    let corps: Value = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    assert_eq!(
        code,
        StatusCode::CONFLICT,
        "🔴 #9340 — {code} {}",
        String::from_utf8_lossy(&octets)
    );
    assert_eq!(corps["code"], CODE_FICHIER_LECTURE_SEULE, "{corps}");
    assert!(
        corps["error"]
            .as_str()
            .unwrap_or("")
            .contains("lecture seule"),
        "{corps}"
    );
}
