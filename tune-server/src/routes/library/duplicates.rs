use axum::Json;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use tune_http_types::panne_sql::OuDefautJournalise;

use crate::error::AppError;
use crate::state::AppState;

use super::Pagination;

#[derive(Deserialize)]
pub(super) struct ResolveDuplicate {
    keep_id: i64,
    delete_id: i64,
}

pub(super) async fn list_duplicates(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(100);
    let offset = p.offset.unwrap_or(0);

    let make_ph = |i: usize| match state.backend.engine() {
        Engine::Sqlite => SqliteDialect.placeholder(i),
        Engine::Postgres => PostgresDialect.placeholder(i),
    };

    // Duplicates by audio_hash
    let hash_sql = format!(
        "SELECT t1.id, t1.title, ar1.name, t1.file_path, t1.audio_hash, t1.duration_ms,
                t2.id, t2.file_path, ar2.name
         FROM tracks t1
         JOIN tracks t2 ON t1.audio_hash = t2.audio_hash AND t1.id < t2.id
         LEFT JOIN artists ar1 ON t1.artist_id = ar1.id
         LEFT JOIN artists ar2 ON t2.artist_id = ar2.id
         WHERE t1.audio_hash IS NOT NULL AND t1.audio_hash != ''
         LIMIT {lim} OFFSET {off}",
        lim = make_ph(1),
        off = make_ph(2),
    );
    let limit_val = limit as i64;
    let offset_val = offset as i64;
    let hash_params: &[&dyn ToSqlValue] = &[&limit_val, &offset_val];
    let hash_rows = state
        .backend
        .query_many(&hash_sql, hash_params)
        .ou_defaut_journalise();
    let hash_dups: Vec<Value> = hash_rows
        .iter()
        .filter_map(|row| {
            let file_path = row.get(3).and_then(|v| v.as_string())?;
            let duplicate_path = row.get(7).and_then(|v| v.as_string())?;
            if !tune_core::scanner::hasher::files_are_byte_identical(
                std::path::Path::new(&file_path),
                std::path::Path::new(&duplicate_path),
            )
            .unwrap_or(false)
            {
                return None;
            }
            Some(json!({
                "id": row.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                "title": row.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": row.get(2).and_then(|v| v.as_string()),
                "file_path": file_path,
                "audio_hash": row.get(4).and_then(|v| v.as_string()),
                "duration_ms": row.get(5).and_then(|v| v.as_i64()).unwrap_or(0),
                "dup_id": row.get(6).and_then(|v| v.as_i64()).unwrap_or(0),
                "dup_path": duplicate_path,
                "dup_artist_name": row.get(8).and_then(|v| v.as_string()),
                "match_type": "audio_hash",
            }))
        })
        .collect();

    // Duplicates by (title + artist_name + duration_ms) where no hash match
    let meta_sql = format!(
        "SELECT t1.id, t1.title, ar1.name, t1.file_path, t1.duration_ms,
                t2.id, t2.file_path, ar2.name
         FROM tracks t1
         JOIN tracks t2 ON LOWER(t1.title) = LOWER(t2.title)
                       AND t1.duration_ms = t2.duration_ms
                       AND t1.id < t2.id
         LEFT JOIN artists ar1 ON t1.artist_id = ar1.id
         LEFT JOIN artists ar2 ON t2.artist_id = ar2.id
         WHERE LOWER(ar1.name) = LOWER(ar2.name)
           AND (t1.audio_hash IS NULL OR t2.audio_hash IS NULL OR t1.audio_hash != t2.audio_hash)
         LIMIT {lim} OFFSET {off}",
        lim = make_ph(1),
        off = make_ph(2),
    );
    let meta_params: &[&dyn ToSqlValue] = &[&limit_val, &offset_val];
    let meta_rows = state
        .backend
        .query_many(&meta_sql, meta_params)
        .ou_defaut_journalise();
    let meta_dups: Vec<Value> = meta_rows
        .iter()
        .map(|row| {
            json!({
                "id": row.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                "title": row.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": row.get(2).and_then(|v| v.as_string()),
                "file_path": row.get(3).and_then(|v| v.as_string()),
                "duration_ms": row.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                "dup_id": row.get(5).and_then(|v| v.as_i64()).unwrap_or(0),
                "dup_path": row.get(6).and_then(|v| v.as_string()),
                "dup_artist_name": row.get(7).and_then(|v| v.as_string()),
                "match_type": "metadata",
            })
        })
        .collect();

    let fp_groups =
        tune_core::library::duplicate_detector::scan_fingerprint_duplicates(&state.backend);
    let fp_dups: Vec<Value> = fp_groups
        .iter()
        .map(|g| {
            json!({
                "fingerprint": g.hash,
                "tracks": g.tracks.iter().map(|t| json!({
                    "id": t.id, "title": t.title, "artist": t.artist_name, "file_path": t.file_path,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();

    Ok(Json(json!({
        "duplicates": {
            "by_hash": hash_dups,
            "by_metadata": meta_dups,
            "by_fingerprint": fp_dups,
        },
        "total": hash_dups.len() + meta_dups.len() + fp_dups.len(),
    })))
}

pub(super) async fn resolve_duplicate(
    State(state): State<AppState>,
    Json(body): Json<ResolveDuplicate>,
) -> Result<impl IntoResponse, AppError> {
    let make_ph = |i: usize| match state.backend.engine() {
        Engine::Sqlite => SqliteDialect.placeholder(i),
        Engine::Postgres => PostgresDialect.placeholder(i),
    };

    // Verify both tracks exist
    let keep_exists = state
        .backend
        .query_one(
            &format!("SELECT COUNT(*) FROM tracks WHERE id = {}", make_ph(1)),
            &[&body.keep_id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
        > 0;

    let delete_exists = state
        .backend
        .query_one(
            &format!("SELECT COUNT(*) FROM tracks WHERE id = {}", make_ph(1)),
            &[&body.delete_id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
        > 0;

    if !keep_exists || !delete_exists {
        return Err(AppError::not_found("track not found"));
    }

    // Reassign playlist references from deleted track to kept track
    state
        .backend
        .execute(
            &format!(
                "UPDATE playlist_tracks SET track_id = {} WHERE track_id = {}",
                make_ph(1),
                make_ph(2)
            ),
            &[&body.keep_id as &dyn ToSqlValue, &body.delete_id],
        )
        .ok();

    // Reassign play queue references (local rows only — streaming rows have
    // track_id NULL and are unaffected).
    state
        .backend
        .execute(
            &format!(
                "UPDATE queue_items SET track_id = {} WHERE track_id = {}",
                make_ph(1),
                make_ph(2)
            ),
            &[&body.keep_id as &dyn ToSqlValue, &body.delete_id],
        )
        .ok();

    // Reassign listen history references
    state
        .backend
        .execute(
            &format!(
                "UPDATE listen_history SET track_id = {} WHERE track_id = {}",
                make_ph(1),
                make_ph(2)
            ),
            &[&body.keep_id as &dyn ToSqlValue, &body.delete_id],
        )
        .ok();

    // Reassign bookmarks
    state
        .backend
        .execute(
            &format!(
                "UPDATE bookmarks SET track_id = {} WHERE track_id = {}",
                make_ph(1),
                make_ph(2)
            ),
            &[&body.keep_id as &dyn ToSqlValue, &body.delete_id],
        )
        .ok();

    // Reassign favorites
    state
        .backend
        .execute(
            &format!(
                "UPDATE favorites SET item_id = {} WHERE item_type = 'track' AND item_id = {}",
                make_ph(1),
                make_ph(2)
            ),
            &[&body.keep_id as &dyn ToSqlValue, &body.delete_id],
        )
        .ok();

    // Delete the duplicate track
    state
        .backend
        .execute(
            &format!("DELETE FROM tracks WHERE id = {}", make_ph(1)),
            &[&body.delete_id as &dyn ToSqlValue],
        )
        .ok();

    Ok(Json(json!({
        "kept": body.keep_id,
        "deleted": body.delete_id,
    })))
}

pub(super) async fn smart_duplicates(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(100);
    let offset = p.offset.unwrap_or(0);

    let make_ph = |i: usize| match state.backend.engine() {
        Engine::Sqlite => SqliteDialect.placeholder(i),
        Engine::Postgres => PostgresDialect.placeholder(i),
    };

    let sql = format!(
        "SELECT t1.id, t1.title, ar1.name, t1.file_path, t1.duration_ms, t1.format, t1.sample_rate, t1.bit_depth, \
                t2.id, t2.file_path, t2.duration_ms, t2.format, t2.sample_rate, t2.bit_depth, ar2.name \
         FROM tracks t1 \
         JOIN tracks t2 ON LOWER(t1.title) = LOWER(t2.title) AND t1.id < t2.id \
         LEFT JOIN artists ar1 ON t1.artist_id = ar1.id \
         LEFT JOIN artists ar2 ON t2.artist_id = ar2.id \
         WHERE LOWER(ar1.name) = LOWER(ar2.name) \
           AND ABS(COALESCE(t1.duration_ms,0) - COALESCE(t2.duration_ms,0)) < 3000 \
         LIMIT {lim} OFFSET {off}",
        lim = make_ph(1),
        off = make_ph(2),
    );

    let limit_val = limit as i64;
    let offset_val = offset as i64;
    let params: &[&dyn ToSqlValue] = &[&limit_val, &offset_val];
    let rows = state
        .backend
        .query_many(&sql, params)
        .ou_defaut_journalise();

    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "track_a": {
                    "id": row.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                    "title": row.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    "artist": row.get(2).and_then(|v| v.as_string()),
                    "file_path": row.get(3).and_then(|v| v.as_string()),
                    "duration_ms": row.get(4).and_then(|v| v.as_i64()),
                    "format": row.get(5).and_then(|v| v.as_string()),
                    "sample_rate": row.get(6).and_then(|v| v.as_i64()),
                    "bit_depth": row.get(7).and_then(|v| v.as_i64()),
                },
                "track_b": {
                    "id": row.get(8).and_then(|v| v.as_i64()).unwrap_or(0),
                    "file_path": row.get(9).and_then(|v| v.as_string()),
                    "duration_ms": row.get(10).and_then(|v| v.as_i64()),
                    "format": row.get(11).and_then(|v| v.as_string()),
                    "sample_rate": row.get(12).and_then(|v| v.as_i64()),
                    "bit_depth": row.get(13).and_then(|v| v.as_i64()),
                    "artist": row.get(14).and_then(|v| v.as_string()),
                },
            })
        })
        .collect();

    Ok(Json(json!({
        "duplicates": items,
        "count": items.len(),
    })))
}

#[derive(Deserialize)]
pub(super) struct ScanParams {
    /// Plafond de pistes examinées. 0 (défaut) = toute la bibliothèque.
    limit: Option<usize>,
}

/// POST /library/duplicates/scan
///
/// Recalcule les empreintes audio manquantes et rend le compte des doublons.
///
/// `list_duplicates` ne rapproche que des pistes dont `audio_hash` est déjà
/// renseigné. Le scanner le calcule à l'indexation
/// (`scanner/walker.rs`), mais rien ne rattrapait les pistes entrées avant
/// que le hachage existe, ni celles dont il avait échoué : elles restaient
/// invisibles aux doublons pour toujours, sans que rien ne le signale.
///
/// Le moteur `duplicate_detector::scan_duplicates` fait exactement ce
/// rattrapage — il calcule l'empreinte absente, la PERSISTE, puis regroupe.
/// Il était complet dans `tune-core` et **n'avait aucun appelant** : une
/// porte HTTP manquait, et l'interface appelait `/metadata/duplicates/scan`,
/// qui n'a jamais existé (#1893).
///
/// Les deux champs rendus sont ceux que lit l'écran Métadonnées ; `groups`
/// n'est pas renvoyé, la liste détaillée étant le travail de
/// `GET /library/duplicates` qui la pagine.
pub(super) async fn scan_duplicates(
    State(state): State<AppState>,
    Query(p): Query<ScanParams>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(0);

    // Le scan lit et réécrit chaque fichier sans empreinte : sur une grande
    // bibliothèque il tient la durée d'un calcul de hachage par piste. Le
    // sortir du fil de la requête évite de bloquer l'exécuteur asynchrone.
    let backend = state.backend.clone();
    let resultat = tokio::task::spawn_blocking(move || {
        tune_core::library::duplicate_detector::scan_duplicates(&backend, limit)
    })
    .await
    .map_err(|e| AppError::internal(format!("scan de doublons interrompu : {e}")))?;

    Ok(Json(json!({
        "total_scanned": resultat.total_scanned,
        "duplicates_found": resultat.duplicates_found,
        "groups": resultat.groups.len(),
        "errors": resultat.errors,
    })))
}
