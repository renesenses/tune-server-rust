//! #5998 — BANC de la vue Répertoires sur une grande bibliothèque. `#[ignore]` :
//! il bâtit une base SQLite FICHIER de 560 000 pistes (des lignes, aucun
//! fichier audio) et ne se lance qu'à la main, en `--release` :
//!
//! ```text
//! cargo test --release -p tune-server --lib banc_repertoires_5998 -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Il MESURE `GET /library/browse/dir` sur un dossier de 14 pistes, comme
//! celui de Tades (fil 2185, 553 501 pistes) : médiane de 7 appels après un
//! appel de chauffe, puis à froid (une écriture avant chaque appel). Le même
//! fichier sert à mesurer AVANT et APRÈS le correctif.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::state::AppState;

const PISTES_DE_FOND: i64 = 560_000;

async fn une_fois(app: &axum::Router, uri: &str) -> (Duration, serde_json::Value) {
    let debut = Instant::now();
    let resp = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (debut.elapsed(), serde_json::from_slice(&octets).unwrap())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "banc manuel, --release"]
async fn banc_repertoires_5998() {
    let dossier = tempfile::tempdir().unwrap();
    let racine = dossier.path().join("musique");
    let album = racine
        .join("x a ecouter")
        .join("Ryan Adams Live at Das Haus on 2006-10-17");
    std::fs::create_dir_all(&album).unwrap();
    let base = dossier.path().join("banc.db");
    let state = AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap();
    let racine_txt = racine.to_str().unwrap().to_string();
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&[&racine_txt]).unwrap(),
        )
        .unwrap();

    let t = Instant::now();
    let s = std::path::MAIN_SEPARATOR;
    state
        .backend
        .execute_batch(&format!(
            "BEGIN; \
             WITH RECURSIVE g(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM g WHERE i < {PISTES_DE_FOND}) \
             INSERT INTO tracks (title, file_path, format, source, track_number, disc_number) \
             SELECT 'Piste ' || i, '{racine_txt}{s}Artiste ' || (i / 300) || '{s}Album ' || (i / 14) \
             || '{s}' || (i % 14 + 1) || ' - Piste ' || i || '.flac', 'flac', 'local', i % 14 + 1, 1 FROM g; \
             COMMIT;"
        ))
        .unwrap();
    let album_txt = album.to_str().unwrap();
    let mut sql = String::from("BEGIN;");
    for n in 1..=14 {
        sql.push_str(&format!(
            "INSERT INTO tracks (title, file_path, format, source, track_number, disc_number) \
             VALUES ('Live {n}', '{album_txt}{s}{n:02} - Live.flac', 'flac', 'local', {n}, 1);"
        ));
    }
    sql.push_str("COMMIT; ANALYZE;");
    state.backend.execute_batch(&sql).unwrap();
    let n = state
        .backend
        .query_many("SELECT COUNT(*) FROM tracks", &[])
        .unwrap()[0][0]
        .as_i64()
        .unwrap();
    eprintln!("BANC5998 pistes={n} remplissage={:?}", t.elapsed());

    let app = crate::routes::router(state.clone());
    let enc = |p: &str| {
        p.bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect::<String>()
    };
    for (nom, chemin) in [
        ("album_14", album_txt.to_string()),
        (
            "x_a_ecouter",
            album.parent().unwrap().to_str().unwrap().to_string(),
        ),
    ] {
        let uri = format!("/api/v1/library/browse/dir?path={}", enc(&chemin));
        let (_, corps) = une_fois(&app, &uri).await;
        let pistes = corps["tracks"].as_array().map(|a| a.len()).unwrap_or(0);
        let dossiers = corps["directories"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        let mut d = Vec::new();
        for _ in 0..7 {
            d.push(une_fois(&app, &uri).await.0);
        }
        d.sort();
        eprintln!(
            "BANC5998 {nom:<12} pistes={pistes} dossiers={dossiers} chaud mediane={:>8.1} ms min={:>8.1} max={:>8.1}",
            ms(d[3]),
            ms(d[0]),
            ms(d[6])
        );
        let _ = state
            .backend
            .execute_batch("CREATE TABLE IF NOT EXISTS banc_froid (x INTEGER);");
        let mut d = Vec::new();
        for _ in 0..7 {
            state
                .backend
                .execute_batch("INSERT INTO banc_froid VALUES (1);")
                .unwrap();
            d.push(une_fois(&app, &uri).await.0);
        }
        d.sort();
        eprintln!(
            "BANC5998 {nom:<12} froid mediane={:>8.1} ms min={:>8.1} max={:>8.1}",
            ms(d[3]),
            ms(d[0]),
            ms(d[6])
        );
        if nom == "album_14" {
            assert_eq!(pistes, 14, "{corps}");
        }
    }
}
