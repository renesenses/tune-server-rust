//! « Create backup » sur PostgreSQL (#7288) : la route rend une sauvegarde
//! réelle (`pg_dump`) ou une erreur qui dit quoi faire, jamais l'ancien refus.
//! La sauvegarde SQLite reste celle d'avant (témoin en fin de fichier).
//!
//! Le vrai `pg_dump` contre une vraie base est exercé par
//! `pg_7288_creer_une_sauvegarde_sur_postgresql` (`tests/pg_routes_serveur.rs`).

use std::path::{Path, PathBuf};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use axum::response::Response;
use serde_json::Value;
use tower::ServiceExt;

use super::{restauration_postgres_a_faire_a_la_main, sauvegarder_postgres};

const URL: &str = "postgres://tune:secret@db.example:5432/tune";

async fn corps(reponse: Response) -> (StatusCode, Value) {
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), usize::MAX).await.unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

#[cfg(unix)]
fn faux_pg_dump(dossier: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dossier.join("faux_pg_dump.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = \"--file\" ]; then shift; printf 'PGDMP' > \"$1\"; fi\n  shift\ndone\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[cfg(unix)]
#[tokio::test]
async fn creer_une_sauvegarde_postgres_rend_le_dump_7288() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("tune.db");
    let pg_dump = faux_pg_dump(tmp.path());

    let (statut, json) =
        corps(sauvegarder_postgres(Some(URL.into()), db_path.to_str().unwrap(), pg_dump).await)
            .await;

    assert_eq!(statut, StatusCode::OK, "{json}");
    assert_eq!(json["engine"], "postgres");
    let nom = json["filename"].as_str().unwrap();
    assert!(
        nom.starts_with("tune_pg_") && nom.ends_with(".dump"),
        "{nom}"
    );
    assert!(tmp.path().join("backups").join(nom).exists());
    assert_eq!(json["size"], 5);
}

#[tokio::test]
async fn sans_pg_dump_la_route_dit_quoi_installer_7288() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("tune.db");

    let (statut, json) = corps(
        sauvegarder_postgres(
            Some(URL.into()),
            db_path.to_str().unwrap(),
            tmp.path().join("absent"),
        )
        .await,
    )
    .await;

    assert_eq!(statut, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json["code"], "pg_dump_not_found");
    let message = json["error"].as_str().unwrap();
    assert!(message.contains("TUNE_PG_DUMP"), "{message}");
    assert!(!message.contains("SQLite-only"), "{message}");
    assert!(!message.contains("secret"), "{message}");
}

#[tokio::test]
async fn sans_url_postgres_la_route_le_dit_7288() {
    let tmp = tempfile::tempdir().unwrap();
    let (statut, json) = corps(
        sauvegarder_postgres(
            None,
            tmp.path().join("tune.db").to_str().unwrap(),
            "pg_dump".into(),
        )
        .await,
    )
    .await;
    assert_eq!(statut, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json["code"], "database_url_missing");
}

/// « Restore » sur PostgreSQL : la réponse dit comment remettre le dump, et
/// non plus que les sauvegardes seraient réservées à SQLite.
#[tokio::test]
async fn restaurer_sur_postgres_donne_la_commande_pg_restore_7288() {
    let (statut, json) = corps(restauration_postgres_a_faire_a_la_main(
        "tune_pg_20261107_120000.dump",
    ))
    .await;

    assert_eq!(statut, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["code"], "restore_with_pg_restore");
    let message = json["error"].as_str().unwrap();
    assert!(
        message.contains("tune_pg_20261107_120000.dump"),
        "{message}"
    );
    assert!(!message.contains("SQLite-only"), "{message}");
    assert!(
        json["restore_hint"]
            .as_str()
            .unwrap()
            .starts_with("pg_restore"),
        "{json}"
    );
}

/// Témoin SQLite : la route copie toujours le fichier de base, sans pg_dump.
#[tokio::test]
async fn sqlite_creer_une_sauvegarde_copie_toujours_le_fichier_7288() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("tune.db");
    let state = crate::state::AppState::new(
        db_path.to_str().unwrap(),
        0,
        crate::config::TuneConfig {
            db_path: db_path.to_string_lossy().into_owned(),
            ..Default::default()
        },
    )
    .expect("AppState SQLite");
    let app = crate::routes::router(state);

    let reponse = app
        .clone()
        .oneshot(
            Request::post("/api/v1/system/backups")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (statut, json) = corps(reponse).await;

    assert_eq!(statut, StatusCode::OK, "{json}");
    let nom = json["filename"].as_str().unwrap();
    assert!(nom.starts_with("tune_") && nom.ends_with(".db"), "{nom}");
    assert!(json.get("engine").is_none(), "{json}");

    let liste = app
        .oneshot(
            Request::get("/api/v1/system/backups")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (_, liste) = corps(liste).await;
    assert_eq!(liste[0]["filename"], nom);
}
