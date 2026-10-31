//! #7539 — la route de modification en masse : réglage « Écrire dans les
//! fichiers » coupé = AUCUN fichier ouvert ; coché = le seul champ modifié
//! réécrit en tâche de fond ; aperçu périmé et filtre vide refusés.
//!
//! Sur des COPIES des fixtures de `tune-core` dans un dossier `test_scratch`
//! — jamais un fichier de musique de qui que ce soit.
use super::*;
use std::path::{Path, PathBuf};

use axum::extract::{RawQuery, State};
use lofty::config::ParseOptions;
use lofty::file::AudioFile;
use tune_core::db::backend::ToSqlValue;
use tune_core::test_scratch::{ScratchDir, scratch_dir};

fn fixture(nom: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tune-core/tests/fixtures")
        .join(nom)
}

struct Banc {
    state: AppState,
    dir: ScratchDir,
}

/// Un album de deux FLAC (copies de la fixture), genre Swing7539.
fn banc(ecriture: bool) -> Banc {
    let dir = scratch_dir("tune-7539-edition");
    let state = AppState::new(
        &dir.join("tune.db").to_string_lossy(),
        0,
        Default::default(),
    )
    .unwrap();
    if ecriture {
        crate::routes::ecriture_fichiers::activer_pour_test(&state.backend);
    }
    let b = &state.backend;
    b.execute("INSERT INTO artists (id, name) VALUES (1, 'Ella')", &[])
        .unwrap();
    b.execute(
        "INSERT INTO albums (id, title, artist_id, genre, genres) \
         VALUES (1, 'Ella 7539', 1, 'Swing7539', '[\"Swing7539\"]')",
        &[],
    )
    .unwrap();
    for (id, nom) in [(11i64, "01.flac"), (12, "02.flac")] {
        let dst = dir.join(nom);
        std::fs::copy(fixture("test.flac"), &dst).unwrap();
        b.execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
             duration_ms, file_path, source, genre, genres, format, sample_rate) \
             VALUES (?, ?, 1, 1, 1, ?, 1000, ?, 'local', 'Swing7539', '[\"Swing7539\"]', 'flac', 44100)",
            &[
                &id as &dyn ToSqlValue,
                &nom.to_string(),
                &(id - 10),
                &dst.to_string_lossy().into_owned(),
            ],
        )
        .unwrap();
    }
    Banc { state, dir }
}

async fn corps(r: Response) -> (StatusCode, Value) {
    let st = r.status();
    let o = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (st, serde_json::from_slice(&o).unwrap_or(Value::Null))
}

async fn voir(state: &AppState, q: &str) -> (StatusCode, Value) {
    corps(
        apercu(State(state.clone()), RawQuery(Some(q.to_string())))
            .await
            .unwrap_or_else(|_| panic!("aperçu : AppError")),
    )
    .await
}

async fn poser(state: &AppState, q: &str, c: Value) -> (StatusCode, Value) {
    let c: CorpsEdition = serde_json::from_value(c).unwrap();
    corps(
        appliquer(State(state.clone()), RawQuery(Some(q.to_string())), Json(c))
            .await
            .unwrap_or_else(|_| panic!("application : AppError")),
    )
    .await
}

fn octets(dir: &Path) -> Vec<Vec<u8>> {
    ["01.flac", "02.flac"]
        .iter()
        .map(|n| std::fs::read(dir.join(n)).unwrap())
        .collect()
}

fn genres_flac(p: &Path) -> Vec<String> {
    let f =
        lofty::flac::FlacFile::read_from(&mut std::fs::File::open(p).unwrap(), ParseOptions::new())
            .unwrap();
    f.vorbis_comments()
        .map(|vc| vc.get_all("GENRE").map(str::to_string).collect())
        .unwrap_or_default()
}

async fn attendre_la_fin(state: &AppState) -> Value {
    for _ in 0..400 {
        let Json(v) = statut_fichiers(State(state.clone())).await;
        if v["status"] != "running" && v["status"] != "idle" {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("la passe des fichiers ne s'est pas terminée");
}

/// 🔴 Réglage coupé (le défaut) : la base change, AUCUN fichier n'est ouvert
/// en écriture — octets identiques, aucune passe lancée, aucun statut.
#[tokio::test(flavor = "multi_thread")]
async fn reglage_coupe_aucun_fichier_n_est_ecrit() {
    let b = banc(false);
    let avant = octets(&b.dir);
    let (st, v) = voir(
        &b.state,
        "genre=Swing7539&item_type=track&field=genre&value=Jazz",
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["tracks"], 2, "{v}");
    assert_eq!(v["albums"], 1, "{v}");
    assert_eq!(v["file_writes_enabled"], false, "{v}");
    assert_eq!(
        v["files"], 0,
        "réglage coupé : l'aperçu annonce zéro fichier — {v}"
    );

    let (st, v) = poser(
        &b.state,
        "genre=Swing7539",
        json!({"item_type": "track", "field": "genre", "value": "Jazz",
               "expected_tracks": 2, "expected_albums": 1}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["tracks"], 2, "{v}");
    assert!(
        v["files_task_id"].is_null(),
        "aucune passe ne doit partir : {v}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        octets(&b.dir),
        avant,
        "réglage coupé : un fichier de musique a été réécrit"
    );
    let Json(s) = statut_fichiers(State(b.state.clone())).await;
    assert_eq!(s["status"], "idle", "aucun statut de passe : {s}");
    let g = b
        .state
        .backend
        .query_one("SELECT genre FROM tracks WHERE id = 11", &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()));
    assert_eq!(g.as_deref(), Some("Jazz"), "la base, elle, a changé");
}

/// Réglage coché : la passe de fond réécrit le genre (tous les genres) dans
/// les copies, et rend son bilan.
#[tokio::test(flavor = "multi_thread")]
async fn reglage_coche_la_passe_reecrit_le_seul_champ() {
    let b = banc(true);
    let (st, v) = voir(
        &b.state,
        "genre=Swing7539&item_type=album&field=genre&value=Pop%3B%20Rock",
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["files"], 2, "{v}");
    let (st, v) = poser(
        &b.state,
        "genre=Swing7539",
        json!({"item_type": "album", "field": "genre", "value": "Pop; Rock"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["files_task_id"].is_string(), "{v}");
    let fin = attendre_la_fin(&b.state).await;
    assert_eq!(fin["status"], "done", "{fin}");
    assert_eq!(fin["written"], 2, "{fin}");
    for n in ["01.flac", "02.flac"] {
        assert_eq!(genres_flac(&b.dir.join(n)), vec!["Pop", "Rock"], "{n}");
    }
}

/// L'aperçu confirmé ne correspond plus : 409, rien n'est écrit.
#[tokio::test(flavor = "multi_thread")]
async fn apercu_perime_rien_n_est_ecrit() {
    let b = banc(false);
    let (st, v) = poser(
        &b.state,
        "genre=Swing7539",
        json!({"item_type": "track", "field": "year", "value": "1961",
               "expected_tracks": 3, "expected_albums": 1}),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "preview_changed", "{v}");
    let n = b
        .state
        .backend
        .query_one("SELECT COUNT(*) FROM tracks WHERE year = 1961", &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()));
    assert_eq!(n, Some(0));
}

/// Filtre vide, champ inconnu, année invalide : 400 avec un code stable.
#[tokio::test(flavor = "multi_thread")]
async fn demandes_refusees() {
    let b = banc(false);
    for (q, c, code) in [
        (
            "",
            json!({"field": "genre", "value": "Jazz"}),
            "empty_filter",
        ),
        (
            "genre=Swing7539",
            json!({"field": "title", "value": "X"}),
            "bad_field",
        ),
        (
            "genre=Swing7539",
            json!({"field": "year", "value": "61"}),
            "invalid_year",
        ),
        (
            "genre=Swing7539",
            json!({"field": "label", "value": "  "}),
            "empty_value",
        ),
    ] {
        let (st, v) = poser(&b.state, q, c).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{q} : {v}");
        assert_eq!(v["code"], code, "{q} : {v}");
    }
}
